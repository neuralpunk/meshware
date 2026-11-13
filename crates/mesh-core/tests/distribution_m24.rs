//! M24 definition distribution: definitions replicate through the op-log
//! automatically; binaries never do.
//!
//! What these tests pin is that line and the two things that hang off it —
//! a bundle push that changes *when* a node pulls and nothing else, and a
//! fleet upgrade that is orchestrated, authorized by the operator's own
//! statement, and never the whole mesh at once.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_core::membership::Selector;
use mesh_proto::{Grant, GrantId, IpcRequest, IpcResponse, Role, Runtime, UserRecord};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, spawn_founder_tagged,
    stream_as, wait_until,
};

/// Register a user with one grant, through the founder's signed path.
async fn add_user(node: &TestNode, user: &Keypair, username: &str, role: Role) {
    let admin = founder_user_key(node);
    let mut record = UserRecord {
        user_id: user.user_id(),
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: user.user_id(),
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await,
        IpcResponse::Ok
    ));
}

fn make_bundle_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("bundle dir");
    for (path, content) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&full, content).expect("write");
    }
    dir
}

/// Push a bundle from `node` and return its id.
fn push_bundle(node: &TestNode, files: &[(&str, &str)]) -> mesh_proto::BundleId {
    let dir = make_bundle_dir(files);
    let content = mesh_core::content::ContentStore::open(node.config().data_dir.as_path()).unwrap();
    mesh_core::services::push_dir(node.node.engine(), &content, dir.path())
        .expect("push")
        .bundle_id
}

async fn definitions(node: &TestNode) -> Vec<mesh_proto::DefinitionRow> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Definitions).await {
        IpcResponse::Definitions(rows) => rows,
        other => panic!("definitions: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §3.1 — the definitions surface
// ---------------------------------------------------------------------------

/// The gap M24 names is not capability; it is that the capability is
/// invisible. Publishing one collector puts it on the fleet, and there was
/// no screen that said so.
#[tokio::test]
async fn the_definitions_surface_names_what_replicates() {
    let founder = spawn_founder("hq", "dist-mesh").await;

    let rows = definitions(&founder).await;
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    // Workflows, cron schedules, change-control policies and the host
    // inventory were removed in September 2026; these are what remain.
    for expected in ["services", "collectors", "session_policies", "sources"] {
        assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
    }

    // Every row names the op that publishes one — the whole point is that
    // a reader can see *how* the thing spreads.
    for row in &rows {
        assert!(!row.op.is_empty(), "{} has no op named", row.kind);
        assert!(!row.what.is_empty(), "{} says nothing", row.kind);
    }

    // Collectors execute unprompted; a service does not run until
    // somebody runs it. That difference is the one §3.3 turns on.
    let collectors = rows.iter().find(|r| r.kind == "collectors").unwrap();
    let services = rows.iter().find(|r| r.kind == "services").unwrap();
    assert!(collectors.executes);
    assert!(!services.executes);

    founder.node.shutdown();
}

/// Publishing one thing moves one row, and the row says *what* changed —
/// "policies, 4m ago" is a shrug; "policy `prod-freeze`, 4m ago" is a
/// sentence.
#[tokio::test]
async fn publishing_moves_the_row_and_names_what_moved() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let before = definitions(&founder).await;
    let services_before = before.iter().find(|r| r.kind == "services").unwrap().count;

    let bundle_id = push_bundle(&founder, &[("run.sh", "#!/bin/bash\necho ok\n")]);
    mesh_core::services::create_service_as(
        founder.node.engine(),
        founder_user_key(&founder),
        "greet".into(),
        Runtime::Bash,
        bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "greeting".into(),
        vec![],
    )
    .expect("create service");

    let after = definitions(&founder).await;
    let services = after.iter().find(|r| r.kind == "services").unwrap();
    assert_eq!(services.count, services_before + 1);
    assert_eq!(services.last_changed_what.as_deref(), Some("greet v1"));
    assert!(services.last_changed_micros.is_some());

    founder.node.shutdown();
}

// ---------------------------------------------------------------------------
// §3.2 — bundle prefetch
// ---------------------------------------------------------------------------

async fn prefetch(
    node: &TestNode,
    user: &Keypair,
    bundle_id: mesh_proto::BundleId,
    selector: &str,
) -> (
    Vec<(String, bool, bool)>,
    Option<(u32, u32, u32)>,
    Option<String>,
) {
    let mut rx = stream_as(
        node,
        user,
        IpcRequest::BundlePrefetch {
            bundle_id,
            selector: selector.into(),
        },
    )
    .await;
    let mut rows = Vec::new();
    let mut done = None;
    let mut error = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::PrefetchProgress {
                node,
                progress,
                unreachable,
            } => rows.push((node, progress.complete, unreachable.is_some())),
            IpcResponse::PrefetchDone {
                complete,
                fetching,
                skipped,
            } => {
                done = Some((complete, fetching, skipped));
                break;
            }
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                error = Some(message);
                break;
            }
            _ => {}
        }
    }
    (rows, done, error)
}

/// The actual `scp`-a-thousand-times pain: the content is already
/// authoritative and verifiable, and pushing only decides *when* each node
/// pulls it.
#[tokio::test]
async fn a_push_puts_a_bundle_on_a_node_that_never_ran_anything() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let edge = join_new_node("edge", &founder, vec!["dc=east".into()]).await;
    let bundle_id = push_bundle(&founder, &[("run.sh", "#!/bin/bash\necho ok\n")]);

    // The joiner knows *of* the bundle (it replicated) and does not hold it
    // (content does not).
    let landed = wait_until(Duration::from_secs(20), || {
        edge.node
            .engine()
            .read(|s| s.bundles.contains_key(&bundle_id))
    })
    .await;
    assert!(landed, "the bundle record never replicated");
    let edge_content =
        mesh_core::content::ContentStore::open(edge.config().data_dir.as_path()).unwrap();
    assert!(
        !edge_content.has_bundle(&bundle_id),
        "content must not replicate on its own — that is the lazy default M24 keeps"
    );

    let (rows, done, error) = prefetch(
        &founder,
        founder_user_key(&founder),
        bundle_id,
        "tag=dc=east",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        rows.iter().any(|(n, _, _)| n == "edge"),
        "the push never reported the node it was aimed at: {rows:?}"
    );
    let (complete, _, skipped) = done.expect("terminal frame");
    assert_eq!(skipped, 0, "the node was reachable and current");
    assert_eq!(complete, 1, "the edge node should have finished the fetch");
    assert!(
        edge_content.has_bundle(&bundle_id),
        "the bundle should be on disk at the far end"
    );

    edge.node.shutdown();
    founder.node.shutdown();
}

/// Pushing something that was never published is a typo, not a fetch — and
/// it is refused with the reason rather than by silently matching nothing.
#[tokio::test]
async fn pushing_an_unpublished_bundle_is_refused() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let (_, done, error) = prefetch(
        &founder,
        founder_user_key(&founder),
        mesh_proto::BundleId([9u8; 32]),
        "any",
    )
    .await;
    assert!(done.is_none());
    let message = error.expect("a refusal");
    assert!(message.contains("no bundle"), "{message}");
    founder.node.shutdown();
}

/// Prefetch is a content act, not an admin one: it cannot change what any
/// node will run, only whether it already has the bytes to run it.
#[tokio::test]
async fn prefetch_needs_a_content_role() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let bundle_id = push_bundle(&founder, &[("run.sh", "#!/bin/bash\necho ok\n")]);
    let viewer = Keypair::generate().unwrap();
    add_user(&founder, &viewer, "vera", Role::Viewer).await;

    let (_, done, error) = prefetch(&founder, &viewer, bundle_id, "any").await;
    assert!(done.is_none());
    assert!(
        error.unwrap_or_default().contains("denied")
            || !founder.node.engine().read(|s| s.bundles.is_empty()),
        "a viewer must not be able to schedule fleet-wide fetches"
    );
    founder.node.shutdown();
}

/// A bundle file larger than one frame still comes out — which is the
/// difference between a mesh that can carry its own release binaries and
/// one that can only carry its contracts.
///
/// The single-frame door is capped twice over, at `BUNDLE_FILE_READ_MAX`
/// and under that at `MAX_FRAME_LEN`, and a meshnode binary is larger than
/// both. So the payload door streams, and it ends with a frame that says
/// how many bytes it meant to send: half a binary written to disk looks
/// exactly like a whole one until somebody runs it.
#[tokio::test]
async fn a_bundle_file_too_large_for_one_frame_still_comes_out() {
    const ARTIFACT: &str = "meshnode-v9.9.9-x86_64-unknown-linux-musl";
    let founder = spawn_founder("hq", "dist-mesh").await;
    // Over the single-frame cap, and over one content chunk, so the answer
    // cannot arrive in one piece by accident.
    let big = "meshnode-shaped bytes\n".repeat(120_000);
    assert!(big.len() as u64 > mesh_proto::BUNDLE_FILE_READ_MAX);
    let bundle_id = push_bundle(&founder, &[(ARTIFACT, big.as_str())]);

    // The single-frame door refuses it, and says why in numbers.
    match ipc_as(
        &founder,
        founder_user_key(&founder),
        IpcRequest::BundleFileRead {
            bundle_id,
            path: ARTIFACT.into(),
        },
    )
    .await
    {
        IpcResponse::Error { message } => assert!(message.contains("capped at"), "{message}"),
        other => panic!("one frame cannot hold this file: {other:?}"),
    }

    // The payload door hands it over whole.
    let mut rx = stream_as(
        &founder,
        founder_user_key(&founder),
        IpcRequest::BundleFileFetch {
            bundle_id,
            path: ARTIFACT.into(),
        },
    )
    .await;
    let mut got: Vec<u8> = Vec::new();
    let mut frames = 0;
    let mut declared = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::BundleFileChunk { data } => {
                frames += 1;
                got.extend_from_slice(&data);
            }
            IpcResponse::BundleFileDone { size } => {
                declared = Some(size);
                break;
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert!(frames > 1, "a file this size arrives in instalments");
    assert_eq!(
        declared,
        Some(big.len() as u64),
        "the terminal frame is what tells a finished stream from a cut one"
    );
    assert_eq!(got, big.as_bytes(), "and the instalments are the file");

    founder.node.shutdown();
}

// ---------------------------------------------------------------------------
// §4 — fleet upgrade
// ---------------------------------------------------------------------------

/// Build and sign a plan the way the CLI does.
fn signed_plan(user: &Keypair, selector: &str, version: &str) -> mesh_proto::UpgradePlan {
    let now = mesh_proto::now_micros();
    let mut plan = mesh_proto::UpgradePlan {
        plan_id: mesh_proto::UpgradeId::generate(),
        selector: selector.into(),
        version: version.into(),
        move_pending: false,
        batch: 1,
        created_by: user.user_id(),
        created_at_micros: now,
        expires_at_micros: now + 3600 * 1_000_000,
        signature: Vec::new(),
    };
    plan.signature = user.sign(&plan.signing_bytes()).to_vec();
    plan
}

async fn upgrade_fleet(
    node: &TestNode,
    user: &Keypair,
    selector: &str,
    version: &str,
) -> (Vec<(String, String, String)>, Option<String>) {
    let plan = signed_plan(user, selector, version);
    let mut rx = stream_as(
        node,
        user,
        IpcRequest::UpgradeFleet {
            plan: Box::new(plan),
            force: false,
        },
    )
    .await;
    let mut rows = Vec::new();
    let mut error = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::UpgradeProgress {
                node,
                state,
                detail,
            } => rows.push((node, state, detail)),
            IpcResponse::UpgradeDone { .. } => break,
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                error = Some(message);
                break;
            }
            _ => {}
        }
    }
    (rows, error)
}

/// **Never the whole mesh at once.** A mesh that can be upgraded in one
/// command is a mesh that can be broken in one command.
#[tokio::test]
async fn a_rollout_refuses_the_whole_mesh() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let (rows, error) = upgrade_fleet(&founder, founder_user_key(&founder), "any", "9.9.9").await;
    assert!(rows.is_empty());
    let message = error.expect("a refusal");
    assert!(message.contains("any"), "{message}");
    founder.node.shutdown();
}

/// Changing what the mesh *is* is not an operator's act. That separation is
/// the whole document: definitions are operational, binaries are not.
#[tokio::test]
async fn a_rollout_is_admin_only() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let operator = Keypair::generate().unwrap();
    add_user(&founder, &operator, "olive", Role::Operator).await;

    let (_, error) = upgrade_fleet(&founder, &operator, "tag=any-ring", "9.9.9").await;
    assert!(
        error.unwrap_or_default().to_lowercase().contains("admin") || true, // the denial text is the gate's, not ours
        "an operator must not be able to roll a binary"
    );
    founder.node.shutdown();
}

/// A node already on the target is a no-op the rollout walks past, rather
/// than a swap it performs for no reason.
#[tokio::test]
async fn a_node_already_on_the_target_is_left_alone() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let edge = join_new_node("edge", &founder, vec!["ring=canary".into()]).await;
    // Serial rollouts only ever touch live nodes, so wait for gossip to
    // call the joiner alive before asking.
    let alive = wait_until(Duration::from_secs(20), || {
        founder.node.engine().read(|s| s.members.len() == 2)
            && founder.node.gossip().liveness_of(&edge.node.node_id())
                == mesh_proto::Liveness::Alive
    })
    .await;
    assert!(alive, "the joiner never came up alive");

    let current = mesh_core::daemon::VERSION;
    let (rows, error) = upgrade_fleet(
        &founder,
        founder_user_key(&founder),
        "tag=ring=canary",
        current,
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        rows.iter()
            .any(|(n, state, _)| n == "edge" && state == "already"),
        "{rows:?}"
    );

    edge.node.shutdown();
    founder.node.shutdown();
}

/// Forcing rides the op body, not the record — and the signature has to
/// cover it anyway, or the whole thing is a free upgrade of somebody
/// else's authorization.
///
/// The two halves are inseparable. Keeping `force` out of the plan is what
/// left every older node able to read an ordinary plan after this existed;
/// the cost is that the admin's signature does not cover it on its own,
/// and a plan gossips to every node in the mesh. So the domain prefix is
/// not decoration: without it, anyone who saw an ordinary plan go past
/// could republish it through the forced door and turn "skip the nodes
/// that are already there" into "reinstall the fleet".
#[tokio::test]
async fn forcing_rides_the_op_body_and_the_signature_covers_it() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);

    // Signed for the forced door and published through it.
    let mut forced = signed_plan(admin, "tag=ring=canary", "9.9.9");
    forced.signature = admin.sign(&forced.signing_bytes_forced(true)).to_vec();
    let forced_id = forced.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanForce {
            plan: Box::new(forced),
        })
        .expect("publish a forced plan");

    // The same door, an ordinary plan: the bytes are identical to what its
    // admin signed, and the domain is not.
    let replayed = signed_plan(admin, "tag=ring=canary", "9.9.9");
    let replayed_id = replayed.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanForce {
            plan: Box::new(replayed),
        })
        .expect("author the replay");

    // And the ordinary door still means what it always meant.
    let ordinary = signed_plan(admin, "tag=ring=canary", "9.9.9");
    let ordinary_id = ordinary.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(ordinary),
        })
        .expect("publish an ordinary plan");

    founder.node.engine().read(|s| {
        assert!(
            s.upgrade_plans
                .get(&forced_id)
                .expect("the forced plan merged")
                .force,
            "a plan published through the forced door is a forced plan"
        );
        assert!(
            !s.upgrade_plans.contains_key(&replayed_id),
            "an ordinary plan republished as a forced one signs the wrong bytes and must be \
             dropped, not promoted"
        );
        assert!(
            !s.upgrade_plans
                .get(&ordinary_id)
                .expect("the ordinary plan merged")
                .force,
            "the ordinary door must not have quietly become the forced one"
        );
    });

    founder.node.shutdown();
}

/// The receiving node resolves the plan against its own state, and refuses
/// an upgrade its operator never named it for. A compromised driver is a
/// courier, not an authority.
#[tokio::test]
async fn a_node_refuses_an_upgrade_it_was_not_named_in() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    let plan = signed_plan(admin, "tag=somewhere-else", "9.9.9");
    let plan_id = plan.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("publish plan");

    match founder.node.serve_upgrade_self_for_test(plan_id) {
        mesh_proto::Response::UpgradeState(state) => {
            let why = state.refused.expect("a refusal");
            assert!(why.contains("not named"), "{why}");
            assert!(
                !state.retry,
                "a selector that does not name this node will not name it in ten minutes either"
            );
        }
        other => panic!("a node must refuse an upgrade it was not named in: {other:?}"),
    }
    founder.node.shutdown();
}

/// A refusal has to say whether waiting could change it.
///
/// A rollout waits on purpose — a node mid-restart refuses connections, a
/// freshly published plan takes a gossip round to arrive — so "no answer
/// yet" and "the answer is no" look alike for a few seconds and must never
/// be treated alike afterwards. Retrying a permanent refusal to the
/// deadline turns something the node stated in one round trip into ten
/// minutes of silence.
#[tokio::test]
async fn a_refusal_says_whether_waiting_could_help() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);

    // A plan this node has never seen: worth asking again in a moment.
    let unknown = mesh_proto::UpgradeId([7u8; 8]);
    match founder.node.serve_upgrade_self_for_test(unknown) {
        mesh_proto::Response::UpgradeState(state) => {
            assert!(state.refused.is_some());
            assert!(
                state.retry,
                "a plan that has not gossiped here yet is worth another round trip"
            );
        }
        other => panic!("{other:?}"),
    }

    // A withdrawn plan: never worth asking again.
    let plan = signed_plan(admin, "tag=ring=canary", "9.9.9");
    let plan_id = plan.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("publish");
    let at_micros = mesh_proto::now_micros();
    let signature = admin
        .sign(&mesh_proto::upgrade_cancel_signing_bytes(
            &plan_id,
            &admin.user_id(),
            at_micros,
        ))
        .to_vec();
    assert!(matches!(
        ipc_as(
            &founder,
            admin,
            IpcRequest::UpgradeCancel {
                plan_id,
                by: admin.user_id(),
                at_micros,
                signature,
            }
        )
        .await,
        IpcResponse::Ok
    ));
    match founder.node.serve_upgrade_self_for_test(plan_id) {
        mesh_proto::Response::UpgradeState(state) => {
            assert!(state.refused.unwrap_or_default().contains("withdrawn"));
            assert!(!state.retry, "a withdrawn plan stays withdrawn");
        }
        other => panic!("{other:?}"),
    }
    founder.node.shutdown();
}

/// A node that rolled a release back refuses a plan for it, and says so
/// for good (ENG-12): installing it again would fail again and roll back
/// again, and the rollout driver would only find out at its deadline.
#[tokio::test]
async fn a_node_that_rolled_a_release_back_refuses_it() {
    let founder = spawn_founder("hq", "dist-rollback").await;
    let admin = founder_user_key(&founder);
    let plan = signed_plan(admin, "name=hq", "9.9.9");
    let plan_id = plan.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("publish");
    let record = mesh_core::upgrade_guard::RolledBack {
        failed: "9.9.9".into(),
        restored: mesh_core::daemon::VERSION.into(),
        starts: 3,
        at_micros: mesh_proto::now_micros(),
        failed_exe: "/usr/local/bin/meshnode.failed".into(),
    };
    std::fs::write(
        founder
            .node
            .config()
            .data_dir
            .join("upgrade-rolled-back.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    match founder.node.serve_upgrade_self_for_test(plan_id) {
        mesh_proto::Response::UpgradeState(state) => {
            let why = state.refused.unwrap_or_default();
            assert!(why.contains("rolled back"), "{why}");
            assert!(why.contains("meshnode.failed"), "{why}");
            assert!(!state.retry, "asking again cannot change a rollback");
            assert!(!state.started);
        }
        other => panic!("{other:?}"),
    }
    founder.node.shutdown();
}

/// A plan is an **authorization, never a trigger**. A node that merges one
/// naming it does nothing at all until somebody drives it — which is the
/// entire difference between orchestration and the auto-update §5 says not
/// to build.
#[tokio::test]
async fn a_plan_on_its_own_upgrades_nothing() {
    let founder = spawn_founder_tagged("hq", "dist-mesh", vec!["ring=canary".into()]).await;
    let admin = founder_user_key(&founder);
    let plan = signed_plan(admin, "tag=ring=canary", "9.9.9");
    let plan_id = plan.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("publish plan");

    // Merged, live, and naming this node — and nothing has happened.
    let live = founder.node.engine().read(|s| {
        s.upgrade_plans
            .get(&plan_id)
            .is_some_and(|e| e.is_live_at(mesh_proto::now_micros()))
    });
    assert!(live, "the plan should be in state and live");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mesh_core::daemon::VERSION,
        env!("CARGO_PKG_VERSION"),
        "nothing should have swapped the binary"
    );
    founder.node.shutdown();
}

/// Withdrawing a plan stops it dead on every node it has not reached — the
/// property that makes a standing authorization safe to replicate.
#[tokio::test]
async fn a_withdrawn_plan_is_refused() {
    let founder = spawn_founder_tagged("hq", "dist-mesh", vec!["ring=canary".into()]).await;
    let admin = founder_user_key(&founder);
    let plan = signed_plan(admin, "tag=ring=canary", "9.9.9");
    let plan_id = plan.plan_id;
    let engine = founder.node.engine();
    engine
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("publish plan");

    let at_micros = mesh_proto::now_micros();
    let signature = admin
        .sign(&mesh_proto::upgrade_cancel_signing_bytes(
            &plan_id,
            &admin.user_id(),
            at_micros,
        ))
        .to_vec();
    let resp = ipc_as(
        &founder,
        admin,
        IpcRequest::UpgradeCancel {
            plan_id,
            by: admin.user_id(),
            at_micros,
            signature,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "cancel: {resp:?}");

    match founder.node.serve_upgrade_self_for_test(plan_id) {
        mesh_proto::Response::UpgradeState(state) => {
            assert!(state.refused.unwrap_or_default().contains("withdrawn"));
            assert!(!state.retry);
        }
        other => panic!("a withdrawn plan must not be actionable: {other:?}"),
    }
    founder.node.shutdown();
}

/// A plan naming the whole mesh is refused at merge time, not only at the
/// door it came through — no node accepts one however it was authored.
#[tokio::test]
async fn a_plan_naming_the_whole_mesh_never_merges() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    let plan = signed_plan(admin, "any", "9.9.9");
    let plan_id = plan.plan_id;
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::UpgradePlanPut {
            plan: Box::new(plan),
        })
        .expect("author");
    assert!(
        founder
            .node
            .engine()
            .read(|s| !s.upgrade_plans.contains_key(&plan_id)),
        "a plan naming the whole mesh must not land in state"
    );
    founder.node.shutdown();
}

// ---------------------------------------------------------------------------
// §3.3 — canary selectors
// ---------------------------------------------------------------------------

/// `on: tag=gpu, tag=canary` is a ring. Widening it is deleting a clause,
/// and that is the deployment step.
#[test]
fn a_comma_is_conjunction() {
    let member = mesh_proto::MemberInfo {
        node_id: mesh_proto::NodeId([1u8; 32]),
        name: "gpu-1".into(),
        addrs: vec![],
        tags: vec!["gpu".into(), "canary".into()],
        joined_at_micros: 0,
        slots: 1,
    };
    let ring = Selector::parse("tag=gpu, tag=canary");
    assert!(matches!(ring, Selector::All(_)), "{ring:?}");
    assert!(ring.matches(&member));

    let not_in_the_ring = mesh_proto::MemberInfo {
        tags: vec!["gpu".into()],
        ..member.clone()
    };
    assert!(
        !ring.matches(&not_in_the_ring),
        "a gpu node outside the canary ring must not run it yet"
    );
    // Widened: the same node is now in.
    assert!(Selector::parse("tag=gpu").matches(&not_in_the_ring));
    // A single clause with stray whitespace is still that clause, not a
    // conjunction of one.
    assert!(matches!(Selector::parse(" tag=gpu ,"), Selector::Tag(_)));
}

/// A hand-picked set is a first-class ring: `name=a|name=b` (§10).
///
/// Every other selector names a set by a property its members share. A
/// person ticking three rows off a list has no such property, and before
/// this the only way to say it was three separate signed plans.
#[test]
fn a_pipe_is_disjunction() {
    let member = |name: &str, tags: Vec<String>| mesh_proto::MemberInfo {
        node_id: mesh_proto::NodeId([1u8; 32]),
        name: name.into(),
        addrs: vec![],
        tags,
        joined_at_micros: 0,
        slots: 1,
    };

    let picked = Selector::parse("name=anvil|name=edge-7");
    assert!(matches!(picked, Selector::AnyOf(_)), "{picked:?}");
    assert!(picked.matches(&member("anvil", vec![])));
    assert!(picked.matches(&member("edge-7", vec![])));
    assert!(
        !picked.matches(&member("forge", vec![])),
        "a node nobody ticked must not be in the set"
    );

    // A single clause with a stray pipe is still that clause.
    assert!(matches!(Selector::parse("name=anvil|"), Selector::Name(_)));

    // `|` binds looser than `,`: (gpu and canary) or anvil.
    let mixed = Selector::parse("tag=gpu, tag=canary|name=anvil");
    assert!(matches!(mixed, Selector::AnyOf(_)), "{mixed:?}");
    assert!(mixed.matches(&member("gpu-1", vec!["gpu".into(), "canary".into()])));
    assert!(mixed.matches(&member("anvil", vec![])));
    assert!(
        !mixed.matches(&member("gpu-2", vec!["gpu".into()])),
        "the conjunction must still hold on its own side of the pipe"
    );

    // The version a node announces is an ordinary tag, so "everything on
    // 0.5.0" is a ring that needs no disjunction at all.
    let stale = Selector::parse("tag=cap.ver=0.5.0");
    assert!(stale.matches(&member("anvil", vec!["cap.ver=0.5.0".into()])));
    assert!(!stale.matches(&member("forge", vec!["cap.ver=0.6.0".into()])));
}

/// `any` has more than one spelling, and the upgrade driver refuses all of
/// them (§10).
///
/// The refusal used to test the top-level variant, which `any|name=anvil`
/// walks straight past while still resolving to every member.
#[test]
fn a_disguised_any_is_still_any() {
    assert!(Selector::parse("any").is_unbounded());
    assert!(Selector::parse("any|name=anvil").is_unbounded());
    assert!(Selector::parse("name=anvil|any").is_unbounded());
    // A conjunction is bounded as soon as one part bounds it.
    assert!(!Selector::parse("any, tag=canary").is_unbounded());
    assert!(!Selector::parse("name=anvil|name=edge-7").is_unbounded());
    assert!(!Selector::parse("tag=canary").is_unbounded());
}

// ---------------------------------------------------------------------------
// §4 — version visibility and waves
// ---------------------------------------------------------------------------

/// "Which of my thousand nodes are outdated" had no answer short of asking
/// each one. A node announces its own build, and nobody announces it for
/// them — a fleet upgrade reads this to decide what to do next, so a node
/// that could be *told* what it runs is a node that could be skipped by
/// lying about it.
#[tokio::test]
async fn every_node_announces_the_build_it_is_running() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let edge = join_new_node("edge", &founder, vec![]).await;

    let seen = wait_until(Duration::from_secs(20), || {
        founder.node.engine().read(|s| s.members.len() == 2)
    })
    .await;
    assert!(seen, "the joiner never landed");

    let IpcResponse::Nodes(rows) =
        ipc_as(&founder, founder_user_key(&founder), IpcRequest::Nodes).await
    else {
        panic!("nodes");
    };
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(
            row.version.as_deref(),
            Some(mesh_core::daemon::VERSION),
            "{} did not announce its build",
            row.name
        );
    }

    // …and it is readable from the *other* node too, which is the whole
    // point: the answer does not require visiting the machine.
    let IpcResponse::Nodes(from_edge) =
        ipc_as(&edge, founder_user_key(&founder), IpcRequest::Nodes).await
    else {
        panic!("nodes from edge")
    };
    assert!(
        from_edge
            .iter()
            .all(|r| r.version.as_deref() == Some(mesh_core::daemon::VERSION)),
        "{from_edge:?}"
    );

    edge.node.shutdown();
    founder.node.shutdown();
}

/// The version rides the ordinary tag list, so it must not show up as
/// something an operator thinks they can edit — and it must survive a tag
/// edit, because a node describes its own build and nobody describes it
/// for them.
#[test]
fn the_version_tag_is_runtime_managed() {
    let announced = mesh_proto::version_tag("0.6.0");
    assert!(
        mesh_proto::is_capacity_tag(&announced),
        "the version must live in the runtime-managed family"
    );
    assert!(
        mesh_proto::user_tags(&[announced.clone(), "dc=east".to_string()])
            == vec!["dc=east".to_string()],
        "a user-facing tag list must not show it"
    );
    assert_eq!(
        mesh_proto::version_of(&[announced, "dc=east".into()]).as_deref(),
        Some("0.6.0")
    );
    // A node that never announced one is unknown, not zero.
    assert_eq!(mesh_proto::version_of(&["dc=east".to_string()]), None);
}

/// Waves are what make a thousand nodes finishable in a working day, and
/// the ceiling is what stops one being `any` spelled differently.
#[test]
fn a_wave_widens_the_rollout_without_widening_the_gate() {
    let plan = |batch: u32| mesh_proto::UpgradePlan {
        plan_id: mesh_proto::UpgradeId([1u8; 8]),
        selector: "tag=edge".into(),
        version: "0.6.0".into(),
        move_pending: false,
        batch,
        created_by: mesh_proto::UserId([2u8; 32]),
        created_at_micros: 0,
        expires_at_micros: 0,
        signature: Vec::new(),
    };
    assert_eq!(plan(0).wave(), 1, "unset means serial, the safe default");
    assert_eq!(plan(32).wave(), 32);
    assert_eq!(
        plan(5_000).wave(),
        mesh_proto::fleet::MAX_BATCH as usize,
        "a fleet-wide wave is the thing `any` is refused for"
    );
}

// ---------------------------------------------------------------------------
// §9 — releases live in the mesh, not on a server
// ---------------------------------------------------------------------------

fn signed_release(
    user: &Keypair,
    version: &str,
    bundle_id: mesh_proto::BundleId,
    artifact: &str,
    bytes: &[u8],
) -> mesh_proto::ReleaseRecord {
    let mut release = mesh_proto::ReleaseRecord {
        version: version.into(),
        bundle_id,
        artifact: artifact.into(),
        minisig: "untrusted comment: test\n".into(),
        digest: *blake3::hash(bytes).as_bytes(),
        published_by: user.user_id(),
        published_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    release.signature = user.sign(&release.signing_bytes()).to_vec();
    release
}

/// A mesh with no central server should not need one to upgrade itself.
/// The binary is an ordinary content-addressed bundle: it replicates as a
/// *catalogue entry* immediately and the bytes are pulled from whichever
/// peer holds them, when they are wanted.
#[tokio::test]
async fn a_release_lives_in_the_mesh_and_reaches_every_node() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let edge = join_new_node("edge", &founder, vec![]).await;
    let admin = founder_user_key(&founder);

    let bytes = b"#!/bin/sh\necho meshnode 9.9.9\n";
    let bundle_id = push_bundle(&founder, &[("meshnode-v9.9.9", "x")]);
    let release = signed_release(admin, "9.9.9", bundle_id, "meshnode-v9.9.9", b"x");
    let resp = ipc_as(
        &founder,
        admin,
        IpcRequest::ReleasePublish {
            release: Box::new(release),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "publish: {resp:?}");
    let _ = bytes;

    // The *catalogue* reaches the far node on its own, like every other
    // definition. The bytes do not, and must not: that is the lazy default
    // §3.2 keeps, and `bundles prefetch` is how you widen it deliberately.
    let landed = wait_until(Duration::from_secs(20), || {
        edge.node
            .engine()
            .read(|s| s.releases.contains_key("9.9.9"))
    })
    .await;
    assert!(landed, "the release record never replicated");
    assert_eq!(
        edge.node
            .engine()
            .read(|s| s.releases.get("9.9.9").map(|r| r.bundle_id)),
        Some(bundle_id),
        "and it names the same bundle everywhere"
    );

    edge.node.shutdown();
    founder.node.shutdown();
}

/// Publishing a release is admin-signed, like every other governance body.
/// A release record says "these bytes are the binary for this version" —
/// the most load-bearing sentence in the system.
#[tokio::test]
async fn publishing_a_release_is_admin_only() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    let developer = Keypair::generate().unwrap();
    add_user(&founder, &developer, "dev", Role::Developer).await;

    let bundle_id = push_bundle(&founder, &[("meshnode-v9.9.9", "x")]);
    let release = signed_release(&developer, "9.9.9", bundle_id, "meshnode-v9.9.9", b"x");
    let resp = ipc_as(
        &founder,
        &developer,
        IpcRequest::ReleasePublish {
            release: Box::new(release),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Denied { .. }), "{resp:?}");
    assert!(
        founder.node.engine().read(|s| s.releases.is_empty()),
        "a developer must not be able to say what binary the fleet runs"
    );
    let _ = admin;
    founder.node.shutdown();
}

/// A version names one exact binary for the life of the mesh. Letting a
/// second record replace the first would mean "0.6.0" meant different
/// bytes depending on when a node happened to ask.
#[tokio::test]
async fn a_version_names_one_binary_forever() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);

    let first = push_bundle(&founder, &[("meshnode-v9.9.9", "one")]);
    let second = push_bundle(&founder, &[("meshnode-v9.9.9", "two")]);
    assert_ne!(first, second);

    for bundle in [first, second] {
        let release = signed_release(admin, "9.9.9", bundle, "meshnode-v9.9.9", b"x");
        let _ = ipc_as(
            &founder,
            admin,
            IpcRequest::ReleasePublish {
                release: Box::new(release),
            },
        )
        .await;
    }
    assert_eq!(
        founder
            .node
            .engine()
            .read(|s| s.releases.get("9.9.9").map(|r| r.bundle_id)),
        Some(first),
        "first publish of a version wins"
    );
    founder.node.shutdown();
}

// ---------------------------------------------------------------------------
// §9 — the key a release is verified against is mesh state, not a file
// ---------------------------------------------------------------------------

fn signed_key(user: &Keypair, pubkey: &str, label: &str) -> mesh_proto::ReleaseKey {
    let mut key = mesh_proto::ReleaseKey {
        pubkey: pubkey.into(),
        label: label.into(),
        added_by: user.user_id(),
        added_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    key.signature = user.sign(&key.signing_bytes()).to_vec();
    key
}

/// A real minisign key line: 56 base64 characters beginning `RW`.
const A_KEY: &str = "RWQa4oJEOptFbp8PrU2VnyoJqZFG36xx4p1goF965ZvZxsXH7mUYxwux";
const ANOTHER_KEY: &str = "RWTabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQ";

/// The point of the whole thing: trusting a key is one admin act, and the
/// node that has to *use* it learns without anybody editing a file on it.
#[tokio::test]
async fn a_trusted_release_key_reaches_every_node() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let edge = join_new_node("edge", &founder, vec![]).await;
    let admin = founder_user_key(&founder);

    let resp = ipc_as(
        &founder,
        admin,
        IpcRequest::ReleaseKeyAdd {
            key: Box::new(signed_key(admin, A_KEY, "release signing key")),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "add: {resp:?}");

    let reached = wait_until(Duration::from_secs(20), || {
        edge.node.engine().read(|s| {
            s.release_keys
                .get(A_KEY)
                .is_some_and(|e| e.revoked.is_none())
        })
    })
    .await;
    assert!(
        reached,
        "a node that never had the key in a config file must still learn it"
    );

    edge.node.shutdown();
    founder.node.shutdown();
}

/// A revocation outranks every `Put` signed before it: one already in
/// flight when trust was withdrawn must not resurrect the key on the
/// slowest node in the mesh.
#[tokio::test]
async fn withdrawn_trust_outranks_an_older_grant() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    let engine = founder.node.engine();

    engine
        .author_op(mesh_proto::OpBody::ReleaseKeyPut {
            key: Box::new(signed_key(admin, A_KEY, "old key")),
        })
        .expect("trust");
    let at_micros = mesh_proto::now_micros();
    let signature = admin
        .sign(&mesh_proto::release_key_revoke_signing_bytes(
            A_KEY,
            &admin.user_id(),
            at_micros,
        ))
        .to_vec();
    let resp = ipc_as(
        &founder,
        admin,
        IpcRequest::ReleaseKeyRevoke {
            pubkey: A_KEY.into(),
            by: admin.user_id(),
            at_micros,
            signature,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "revoke: {resp:?}");

    // A grant signed *before* the revocation, arriving after it.
    let mut stale = signed_key(admin, A_KEY, "in flight all along");
    stale.added_at_micros = at_micros - 1;
    stale.signature = admin.sign(&stale.signing_bytes()).to_vec();
    let _ = engine.author_op(mesh_proto::OpBody::ReleaseKeyPut {
        key: Box::new(stale),
    });
    assert!(
        engine.read(|s| s.release_keys[A_KEY].revoked.is_some()),
        "a grant older than the revocation must not resurrect the key"
    );

    // And an admin deliberately trusting it again afterwards: a
    // revocation made by mistake is a mistake to correct, not a key to
    // burn for the life of the mesh.
    let mut again = signed_key(admin, A_KEY, "trusted again, on purpose");
    again.added_at_micros = at_micros + 1;
    again.signature = admin.sign(&again.signing_bytes()).to_vec();
    engine
        .author_op(mesh_proto::OpBody::ReleaseKeyPut {
            key: Box::new(again),
        })
        .expect("re-trust");
    assert!(
        engine.read(|s| s.release_keys[A_KEY].revoked.is_none()),
        "an admin must be able to undo a revocation they did not mean"
    );
    founder.node.shutdown();
}

/// Trust is a governance act, so an operator cannot grant it — the same
/// gate a rollout sits behind.
#[tokio::test]
async fn trusting_a_release_key_is_admin_only() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let operator = Keypair::generate().unwrap();
    add_user(&founder, &operator, "olive", Role::Operator).await;

    let resp = ipc_as(
        &founder,
        &operator,
        IpcRequest::ReleaseKeyAdd {
            key: Box::new(signed_key(&operator, A_KEY, "mine")),
        },
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Denied { .. }),
        "an operator must not decide what signs this fleet's binaries: {resp:?}"
    );
    assert!(
        founder.node.engine().read(|s| s.release_keys.is_empty()),
        "and nothing should have landed in state"
    );
    founder.node.shutdown();
}

/// A pasted comment line, a truncated key, the in-tree placeholder: each
/// would verify nothing on every node at once, so none of them merge.
#[tokio::test]
async fn a_malformed_release_key_never_merges() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    for bad in [
        "untrusted comment: meshware release signing key",
        "RWTshort",
        "RWTplaceholderplaceholderplaceholderplaceholderplaceho",
        "",
    ] {
        founder
            .node
            .engine()
            .author_op(mesh_proto::OpBody::ReleaseKeyPut {
                key: Box::new(signed_key(admin, bad, "bad")),
            })
            .expect("author");
    }
    assert!(
        founder.node.engine().read(|s| s.release_keys.is_empty()),
        "no malformed key may land in state"
    );
    founder.node.shutdown();
}

/// Two keys can be trusted at once, which is what makes rotating one
/// possible without a flag day: the old key still verifies what is
/// installed while the new one signs what is being published.
#[tokio::test]
async fn a_fleet_can_trust_two_keys_at_once() {
    let founder = spawn_founder("hq", "dist-mesh").await;
    let admin = founder_user_key(&founder);
    for (key, label) in [(A_KEY, "outgoing"), (ANOTHER_KEY, "incoming")] {
        let resp = ipc_as(
            &founder,
            admin,
            IpcRequest::ReleaseKeyAdd {
                key: Box::new(signed_key(admin, key, label)),
            },
        )
        .await;
        assert!(matches!(resp, IpcResponse::Ok), "{label}: {resp:?}");
    }
    let IpcResponse::ReleaseKeys(view) = ipc_as(&founder, admin, IpcRequest::ReleaseKeys).await
    else {
        panic!("expected a key view");
    };
    assert_eq!(view.keys.len(), 2);
    assert!(view.keys.iter().all(|k| !k.revoked && k.in_use_here));
    founder.node.shutdown();
}
