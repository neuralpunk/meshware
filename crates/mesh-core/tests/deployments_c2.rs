//! C2 exit criteria: a deployment keeps N instances of a service up, and
//! taking the deployment away takes its instances with it (§6.4 P1).
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::deployments::{Slot, instance_id};
use mesh_proto::{IpcRequest, IpcResponse, Runtime};
use mesh_testkit::{founder_user_key, spawn_founder, wait_until};

fn make_bundle_dir(files: &[(&str, &str, u32)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("bundle dir");
    for (path, content, mode) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&full, content).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(*mode)).expect("chmod");
        }
    }
    dir
}

/// How many of a fixed-count deployment's current-generation instances are
/// non-terminal (queued or running) in replicated state.
fn live_instances(node: &mesh_testkit::TestNode, name: &str, count: u32) -> usize {
    let engine = node.node.engine();
    engine.read(|s| {
        (0..count)
            .filter(|i| {
                let mut found = false;
                for inc in 0..8u32 {
                    let id = instance_id(name, 0, &Slot::Replica(*i), inc);
                    match s.jobs.get(&id) {
                        Some(job) if !job.state.is_terminal() => {
                            found = true;
                            break;
                        }
                        Some(_) => continue,
                        None => break,
                    }
                }
                found
            })
            .count()
    })
}

/// Every incarnation of one slot that `node` has a record of, as
/// `(incarnation, state, executor)`.
fn incarnations(
    node: &mesh_testkit::TestNode,
    name: &str,
    slot: u32,
) -> Vec<(u32, mesh_proto::JobState, mesh_proto::NodeId)> {
    node.node.engine().read(|s| {
        (0..8u32)
            .filter_map(|inc| {
                s.jobs
                    .get(&instance_id(name, 0, &Slot::Replica(slot), inc))
                    .map(|job| (inc, job.state, job.executor))
            })
            .collect()
    })
}

/// Does `node` see a running instance of `slot` executed by `executor`?
fn running_on(
    node: &mesh_testkit::TestNode,
    name: &str,
    slot: u32,
    executor: mesh_proto::NodeId,
) -> bool {
    incarnations(node, name, slot)
        .iter()
        .any(|(_, st, ex)| matches!(st, mesh_proto::JobState::Running) && *ex == executor)
}

/// **Phase A, §2.1–2.2.** An instance on a node that goes away and *stays*
/// away is replaced on a node that answers once `lost_after` passes — with
/// nobody restarting the node, draining it, or force-cancelling anything.
///
/// Before Phase A the slot waited for that node forever by default, and an
/// explicit `lost_after` cancelled into the void: the cancel went to the
/// unreachable executor, the job stayed `running` in replicated state, and
/// nothing refilled the slot.
///
/// `lost_after = "5s"` rather than the 60 s default, so this measures the
/// mechanism rather than the wait. The default is asserted where it is
/// decided, in the parser's unit tests.
///
/// `count = 2` so the spread preference puts one instance on each node —
/// which means both nodes already hold the script's bundle — and it is the
/// joiner that is stopped. A first version used `count = 1` and stopped
/// whichever node ran it, which was usually the founder: the only holder
/// of the bundle, so the replacement could never fetch its code, and the
/// test measured content availability instead of the reconciler.
#[tokio::test]
async fn an_instance_on_a_node_that_stays_down_is_replaced() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = mesh_testkit::join_new_node("beta", &a, vec![]).await;
    let (id_a, id_b) = (a.node.node_id(), b.node.node_id());
    let met = wait_until(Duration::from_secs(30), || {
        a.node.engine().read(|s| s.members.contains_key(&id_b))
            && b.node.engine().read(|s| s.members.contains_key(&id_a))
    })
    .await;
    assert!(met, "beta did not join");

    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");

    let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 2
targets = "any"

[deployment.disconnect]
lost_after = "5s"
"#;
    let parsed = mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap();
    assert_eq!(parsed.policy.lost_after_secs, 5);
    assert!(
        !parsed.policy.replace_unknown,
        "the point is lost_after alone"
    );
    let signed = mesh_core::deployments::sign(founder_user_key(&a), parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(founder_user_key(&a), parsed.policy);
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "put failed: {resp:?}");

    // One instance on each node, and both nodes agree.
    let one_each = |n: &mesh_testkit::TestNode| {
        let on = |who| {
            (0..2u32)
                .filter(|slot| running_on(n, "web", *slot, who))
                .count()
        };
        on(id_a) == 1 && on(id_b) == 1
    };
    let spread = wait_until(Duration::from_secs(90), || one_each(&a) && one_each(&b)).await;
    assert!(
        spread,
        "the two instances did not land one per node: slot 0 {:?}, slot 1 {:?}",
        incarnations(&a, "web", 0),
        incarnations(&a, "web", 1)
    );
    let beta_slot = (0..2u32)
        .find(|slot| running_on(&a, "web", *slot, id_b))
        .expect("a slot running on beta");

    b.stop().await;

    // Alpha still holds beta's instance as `running` — beta never said
    // otherwise and nobody else may close it — so this waits for a
    // *second* running incarnation of that slot, on alpha, beside it.
    let replaced = wait_until(Duration::from_secs(120), || {
        running_on(&a, "web", beta_slot, id_a)
    })
    .await;
    assert!(
        replaced,
        "beta's instance was never replaced on alpha: {:?}",
        incarnations(&a, "web", beta_slot)
    );

    a.node.shutdown();
}

/// Publish a parsed definition from `node`, as its founder.
async fn publish(node: &mesh_testkit::TestNode, parsed: mesh_core::deployments::Parsed) {
    let signed = mesh_core::deployments::sign(founder_user_key(node), parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(founder_user_key(node), parsed.policy);
    let resp = mesh_testkit::ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "put failed: {resp:?}");
}

/// **Phase B, E3.** A resize adds and retires replicas at the same
/// generation, and the instance that was already running is the *same job*
/// before and after — up to three and back down to one. That is the whole
/// difference between `deployment scale` and editing `count` before it
/// existed, which rolled every instance to change how many there were.
#[tokio::test]
async fn scaling_adds_and_retires_instances_without_restarting_the_rest() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");

    let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\n";
    let user = founder_user_key(&a).user_id();
    publish(&a, mesh_core::deployments::parse(src, user).unwrap()).await;
    let first = instance_id("web", 0, &Slot::Replica(0), 0);
    let running = |id| {
        a.node.engine().read(|s| {
            s.jobs
                .get(&id)
                .is_some_and(|j| matches!(j.state, mesh_proto::JobState::Running))
        })
    };
    assert!(
        wait_until(Duration::from_secs(30), || running(first)).await,
        "the first instance did not start"
    );

    let mut up = mesh_core::deployments::parse(src, user).unwrap();
    assert_eq!(mesh_core::deployments::scale(&mut up, 3).unwrap(), 1);
    publish(&a, up).await;
    assert!(
        wait_until(Duration::from_secs(30), || live_instances(&a, "web", 3)
            == 3)
        .await,
        "scaling to three did not bring up two more"
    );
    assert!(
        running(first),
        "the instance that was already up was restarted"
    );

    let mut down = mesh_core::deployments::parse(src, user).unwrap();
    assert_eq!(mesh_core::deployments::scale(&mut down, 1).unwrap(), 1);
    publish(&a, down).await;
    assert!(
        wait_until(Duration::from_secs(30), || live_instances(&a, "web", 3)
            == 1)
        .await,
        "scaling back to one did not retire the other two"
    );
    assert!(running(first), "the instance that stays was restarted");

    a.node.shutdown();
}

async fn history_of(
    node: &mesh_testkit::TestNode,
    name: &str,
) -> mesh_proto::DeploymentHistoryView {
    let resp = mesh_testkit::ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::DeploymentHistory { name: name.into() },
    )
    .await;
    let IpcResponse::DeploymentHistory(view) = resp else {
        panic!("history failed: {resp:?}");
    };
    *view
}

/// **Phase B, E1.** Every publish is a version, kept with its side-car; a tag
/// names one; and a rollback to the tag brings the whole file back — here the
/// targets an edit changed, which replaces the instance, and the description
/// that travelled with them. A removal ends the history, so a new deployment
/// reusing the name starts with none and no tags. Through the same IPC
/// `deployment history`, `tag` and `rollback --tag` use.
#[tokio::test]
async fn a_deployment_keeps_its_versions_and_goes_back_to_a_tagged_one() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");
    let key = founder_user_key(&a);
    let user = key.user_id();
    let running = |generation: u32| {
        let id = instance_id("web", generation, &Slot::Replica(0), 0);
        a.node.engine().read(|s| {
            s.jobs
                .get(&id)
                .is_some_and(|j| matches!(j.state, mesh_proto::JobState::Running))
        })
    };

    let first =
        "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\ndescription = \"first\"\n";
    publish(&a, mesh_core::deployments::parse(first, user).unwrap()).await;
    let h = history_of(&a, "web").await;
    assert_eq!(h.versions.len(), 1);
    let golden = mesh_core::deployments::find_version(&h, None, None).unwrap();
    let tag = mesh_core::deployments::sign_tag(
        key,
        mesh_proto::DeploymentTag {
            deployment: "web".into(),
            tag: "golden".into(),
            version: Some(golden.clone()),
            description: "known good".into(),
            set_by: user,
            set_at_micros: 0,
            signature: vec![],
        },
    );
    let resp =
        mesh_testkit::ipc_as(&a, key, IpcRequest::DeploymentTag { tag: Box::new(tag) }).await;
    assert!(matches!(resp, IpcResponse::Ok), "tag failed: {resp:?}");

    // An edit that replaces the instance: pinned to this node by name.
    let mut second =
        mesh_core::deployments::parse(&first.replace("\"first\"", "\"second\""), user).unwrap();
    second.deployment.targets = "name=alpha".into();
    second.deployment.generation = 1;
    publish(&a, second).await;
    assert!(
        wait_until(Duration::from_secs(30), || running(1)).await,
        "the edited version did not start"
    );
    let h = history_of(&a, "web").await;
    let generations: Vec<u32> = h.versions.iter().map(|v| v.generation).collect();
    assert_eq!(generations, vec![1, 0]);
    assert!(h.versions[0].current);
    assert_eq!(h.versions[1].tags, vec!["golden".to_string()]);
    let fields: Vec<&str> = h.versions[0]
        .changes
        .iter()
        .map(|c| c.field.as_str())
        .collect();
    assert!(
        fields.contains(&"targets") && fields.contains(&"description"),
        "the edit's changes are listed: {fields:?}"
    );

    // Back to the tag.
    let resp =
        mesh_testkit::ipc_as(&a, key, IpcRequest::DeploymentShow { name: "web".into() }).await;
    let IpcResponse::DeploymentShow(row) = resp else {
        panic!("show failed: {resp:?}");
    };
    let mut back = mesh_core::deployments::parse(row.source.as_deref().unwrap(), user).unwrap();
    let tagged = mesh_core::deployments::find_version(&h, None, Some("golden")).unwrap();
    mesh_core::deployments::revert_to(&mut back, &tagged).unwrap();
    assert_eq!(back.deployment.generation, 2, "different targets roll");
    publish(&a, back).await;
    assert!(
        wait_until(Duration::from_secs(30), || running(2)).await,
        "the version rolled back to did not start"
    );
    let now = a
        .node
        .engine()
        .read(|s| s.deployments["web"].deployment.clone());
    assert_eq!(now.description, "first");
    assert_eq!(now.targets, golden.deployment.targets);
    let generations: Vec<u32> = history_of(&a, "web")
        .await
        .versions
        .iter()
        .map(|v| v.generation)
        .collect();
    assert_eq!(generations, vec![2, 1, 0]);

    // A removal ends it.
    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentRemove {
            name: "web".into(),
            by: user,
            signature: mesh_core::deployments::sign_remove(key, "web"),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "remove failed: {resp:?}");
    publish(&a, mesh_core::deployments::parse(first, user).unwrap()).await;
    let h = history_of(&a, "web").await;
    assert_eq!(h.versions.len(), 1, "a new deployment starts a new history");
    assert!(h.tags.is_empty(), "and the old one's tags went with it");

    a.node.shutdown();
}

/// **Phase B, E3/U1.** A running instance can be looked at up close — its
/// timeline, and what it is using, measured on its node just now — through
/// the same IPC `deployment instance`, `status --stats` and Director use. An
/// instance is a bash loop here, so the numbers come from its cgroup or its
/// process, and the test holds only to what is true of either.
#[tokio::test]
async fn an_instance_reports_its_timeline_and_what_it_uses() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");
    let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\n";
    let user = founder_user_key(&a).user_id();
    publish(&a, mesh_core::deployments::parse(src, user).unwrap()).await;

    let first = instance_id("web", 0, &Slot::Replica(0), 0);
    let up = wait_until(Duration::from_secs(30), || {
        a.node.engine().read(|s| {
            s.jobs
                .get(&first)
                .is_some_and(|j| matches!(j.state, mesh_proto::JobState::Running))
        })
    })
    .await;
    assert!(up, "the instance did not start");

    let key = founder_user_key(&a);
    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentInstance {
            name: "web".into(),
            slot: "0".into(),
        },
    )
    .await;
    let IpcResponse::DeploymentInstance(detail) = resp else {
        panic!("unexpected: {resp:?}");
    };
    assert_eq!(detail.incarnations.len(), 1, "{:?}", detail.incarnations);
    assert_eq!(detail.incarnations[0].job_id, first);
    assert_eq!(detail.current.as_ref().map(|c| c.job_id), Some(first));
    let stat = detail
        .stats
        .expect("a running instance on this node is measured");
    assert!(
        matches!(stat.source.as_str(), "cgroup" | "process"),
        "{stat:?}"
    );
    assert!(stat.mem_bytes.is_some_and(|m| m > 0), "{stat:?}");
    assert!(stat.cpu_pct.is_some(), "{stat:?}");

    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentInstanceStats { name: "web".into() },
    )
    .await;
    let IpcResponse::DeploymentInstanceStats(rows) = resp else {
        panic!("unexpected: {resp:?}");
    };
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].stat.is_some(), "{rows:?}");

    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentInstance {
            name: "web".into(),
            slot: "nine".into(),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Error { .. }), "{resp:?}");

    a.node.shutdown();
}

#[tokio::test]
async fn a_deployment_keeps_its_instances_up() {
    let a = spawn_founder("alpha", "hyacinth").await;

    // A service that stays up: sleep forever, so the instance job is a
    // long-running process rather than one that exits.
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");

    // Publish a deployment of three instances.
    let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 3
targets = "any"
"#;
    let parsed = mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap();
    let signed = mesh_core::deployments::sign(founder_user_key(&a), parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(founder_user_key(&a), parsed.policy);
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "put failed: {resp:?}");

    // The reconciler brings all three up without anyone watching.
    let up = wait_until(Duration::from_secs(30), || {
        live_instances(&a, "web", 3) == 3
    })
    .await;
    assert!(up, "three instances did not come up");

    // Status reports desired/running.
    let status = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentStatus { name: "web".into() },
    )
    .await;
    let IpcResponse::DeploymentStatus(view) = status else {
        panic!("unexpected: {status:?}");
    };
    assert_eq!(view.desired, 3);
    assert!(view.running >= 1);

    // Removing the deployment retires its instances.
    let sig = mesh_core::deployments::sign_remove(founder_user_key(&a), "web");
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentRemove {
            name: "web".into(),
            by: founder_user_key(&a).user_id(),
            signature: sig,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "remove failed: {resp:?}");

    let down = wait_until(Duration::from_secs(30), || {
        live_instances(&a, "web", 3) == 0
    })
    .await;
    assert!(down, "instances were not retired after removal");

    a.node.shutdown();
}

#[tokio::test]
async fn a_health_failing_service_goes_degraded() {
    let a = spawn_founder("gamma", "hyacinth").await;
    // A service that stays up but never answers its health check: the http
    // probe targets a port nothing listens on, so every probe fails.
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "flaky".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "never healthy".into(),
        vec![],
    )
    .expect("create service");

    let src = r#"
[deployment]
name    = "flaky"
service = "flaky@1"
count   = 1

[deployment.restart]
backoff = "1s..2s"
budget  = "2 in 10m"

[deployment.health]
kind     = "http"
port     = 9         # discard port; nothing serves it
path     = "/healthz"
interval = "2s"
grace    = "1s"
timeout  = "1s"
failures = 1
"#;
    let parsed = mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap();
    let signed = mesh_core::deployments::sign(founder_user_key(&a), parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(founder_user_key(&a), parsed.policy);
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "put failed: {resp:?}");

    // The prober kills each unhealthy instance (recorded as a failure), the
    // reconciler restarts it, and after the budget is spent the deployment
    // goes degraded and stops restarting.
    let degraded = wait_until(Duration::from_secs(90), || {
        let engine = a.node.engine();
        let now = mesh_proto::now_micros();
        engine.read(|s| {
            s.deployments
                .get("flaky")
                .map(|e| {
                    let cands: Vec<_> = s.members.values().map(|m| m.info.clone()).collect();
                    mesh_core::deployments::plan_reconcile(
                        &e.deployment,
                        s,
                        &cands,
                        now,
                        &mesh_core::deployments::policy_for(s, &e.deployment.name),
                        |_| None,
                    )
                    .degraded
                    .is_some()
                })
                .unwrap_or(false)
        })
    })
    .await;
    assert!(
        degraded,
        "a health-failing service should trip the budget and go degraded"
    );

    a.node.shutdown();
}

#[tokio::test]
async fn dry_run_flags_a_missing_service() {
    let a = spawn_founder("beta", "hyacinth").await;
    let src = r#"
[deployment]
name    = "ghost"
service = "nope@1"
count   = 1
"#;
    let parsed = mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap();
    let signed = mesh_core::deployments::sign(founder_user_key(&a), parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(founder_user_key(&a), parsed.policy);
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: true,
            inline_service: false,
        },
    )
    .await;
    let IpcResponse::DeploymentCheck(view) = resp else {
        panic!("unexpected: {resp:?}");
    };
    assert!(
        view.issues.iter().any(|i| i.contains("nope")),
        "expected a missing-service issue, got {:?}",
        view.issues
    );

    a.node.shutdown();
}

/// Publish a whole file: its knobs, then the deployment — the order
/// `meshnode deployment put` sends them in.
async fn publish_file(node: &mesh_testkit::TestNode, parsed: mesh_core::deployments::Parsed) {
    let knobs = mesh_core::deployments::sign_knobs(founder_user_key(node), parsed.knobs.clone());
    let resp = mesh_testkit::ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::DeploymentKnobs {
            knobs: Box::new(knobs),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "knobs failed: {resp:?}");
    publish(node, parsed).await;
}

async fn status_of(node: &mesh_testkit::TestNode, name: &str) -> mesh_proto::DeploymentStatusView {
    let resp = mesh_testkit::ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::DeploymentStatus { name: name.into() },
    )
    .await;
    let IpcResponse::DeploymentStatus(view) = resp else {
        panic!("status failed: {resp:?}");
    };
    *view
}

/// **Phase B, E2.** A deployment that set `auto_promote = false` brings its
/// canary up beside the old instances and then *holds* — nothing is cut over
/// however healthy the canary is — until somebody promotes that generation.
/// A promotion naming another generation is refused; the right one lets the
/// roll finish. Through the same IPC `deployment promote` and Director use.
#[tokio::test]
async fn a_held_canary_waits_for_a_person_and_then_the_roll_finishes() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");
    let key = founder_user_key(&a);
    let user = key.user_id();
    let live_at = |generation: u32| {
        a.node.engine().read(|s| {
            (0..2u32)
                .filter(|slot| {
                    (0..8u32).any(|inc| {
                        s.jobs
                            .get(&instance_id("web", generation, &Slot::Replica(*slot), inc))
                            .is_some_and(|j| matches!(j.state, mesh_proto::JobState::Running))
                    })
                })
                .count()
        })
    };

    let src = concat!(
        "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n",
        "[deployment.update]\ncanary = 1\nauto_promote = false\n"
    );
    publish_file(&a, mesh_core::deployments::parse(src, user).unwrap()).await;
    assert!(
        wait_until(Duration::from_secs(30), || live_at(0) == 2).await,
        "the first version did not come up"
    );

    let mut next = mesh_core::deployments::parse(src, user).unwrap();
    next.deployment.generation = 1;
    publish_file(&a, next).await;
    let mut held = false;
    for _ in 0..60 {
        if status_of(&a, "web").await.awaiting_promotion {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(held, "the settled canary was never reported as holding");
    // Well past when an unheld roll would have cut over.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(live_at(0), 2, "an old instance was cut over while held");
    assert_eq!(live_at(1), 1, "more than the canary came up while held");

    let stale = mesh_core::deployments::sign_promotion(key, "web", 7);
    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentPromote {
            promotion: Box::new(stale),
        },
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Error { .. }),
        "a promotion of another generation was accepted: {resp:?}"
    );

    let promotion = mesh_core::deployments::sign_promotion(key, "web", 1);
    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentPromote {
            promotion: Box::new(promotion),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "promote failed: {resp:?}");
    assert!(
        wait_until(Duration::from_secs(60), || live_at(1) == 2
            && live_at(0) == 0)
        .await,
        "the roll did not finish after promotion: {} old, {} new",
        live_at(0),
        live_at(1)
    );

    a.node.shutdown();
}

/// Publish a bash service `web` from `node`.
fn web_service(node: &mesh_testkit::TestNode) {
    let bundle = make_bundle_dir(&[(
        "run.sh",
        "#!/bin/bash\nwhile true; do sleep 1; done\n",
        0o755,
    )]);
    let engine = node.node.engine();
    let content = mesh_core::content::ContentStore::open(node.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(node),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "long-running web".into(),
        vec![],
    )
    .expect("create service");
    // `bundle` must outlive the push only; the content store holds it now.
    drop(bundle);
}

/// Running instances of `web`, generation 0, by executor.
fn running_by_node(node: &mesh_testkit::TestNode, slots: u32) -> Vec<mesh_proto::NodeId> {
    node.node.engine().read(|s| {
        (0..slots)
            .filter_map(|slot| {
                (0..16u32).find_map(|inc| {
                    s.jobs
                        .get(&instance_id("web", 0, &Slot::Replica(slot), inc))
                        .filter(|j| matches!(j.state, mesh_proto::JobState::Running))
                        .map(|j| j.executor)
                })
            })
            .collect()
    })
}

/// **Phase B hardening — the bug as it was found on a real mesh.** Two
/// instances asking for four cores each, on a mesh of an 8-core machine and a
/// 2-core one. The spread preference sends the second instance to the other
/// machine; before the fix that was the 2-core one, which refused it every
/// five seconds forever while the slot read `absent`. A machine that can never
/// fit the request is not a candidate, so both land on the one that can.
#[tokio::test]
async fn an_instance_is_never_planned_onto_a_machine_too_small_for_it() {
    let a =
        mesh_testkit::spawn_founder_sized("big", "fits", mesh_testkit::capacity(8, 32, 100)).await;
    let b =
        mesh_testkit::join_new_node_sized("small", &a, mesh_testkit::capacity(2, 32, 100)).await;
    let id_b = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(30), || a
            .node
            .engine()
            .read(|s| s.members.contains_key(&id_b)))
        .await,
        "small did not join"
    );
    web_service(&a);
    let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.resources]\ncpus = 4\n";
    publish(
        &a,
        mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap(),
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(60), || running_by_node(&a, 2).len()
            == 2)
        .await,
        "both instances did not start: running on {:?}",
        running_by_node(&a, 2)
    );
    assert!(
        running_by_node(&a, 2)
            .iter()
            .all(|n| *n == a.node.node_id()),
        "an instance was placed on the machine with two cores"
    );
    b.node.shutdown();
    a.node.shutdown();
}

/// **Phase B hardening.** A machine that takes one job and refuses the next
/// (`slots = 1`, no queue). Instances offered to it after the first are
/// refused, and a refusal now spills to the next machine instead of being
/// retried against the same one forever: all four instances run.
#[tokio::test]
async fn a_refused_instance_spills_to_the_next_machine() {
    let a = spawn_founder("roomy", "spill").await;
    let b = mesh_testkit::join_new_node_with("cramped", &a, vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 0;
    })
    .await;
    let id_b = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(30), || a
            .node
            .engine()
            .read(|s| s.members.contains_key(&id_b)))
        .await,
        "cramped did not join"
    );
    web_service(&a);
    let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 4\n";
    publish(
        &a,
        mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap(),
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(90), || running_by_node(&a, 4).len()
            == 4)
        .await,
        "not every instance started: running on {:?}",
        running_by_node(&a, 4)
    );
    let on_b = running_by_node(&a, 4)
        .iter()
        .filter(|n| **n == id_b)
        .count();
    assert!(on_b <= 1, "cramped runs {on_b} instances with one slot");
    b.node.shutdown();
    a.node.shutdown();
}

/// **Phase B hardening.** When no machine will take an instance, the slot
/// says why — on every node, because the reason replicates — instead of
/// reading `absent` with nothing beside it. A one-node mesh whose only machine
/// takes one job: the second instance is refused, and its row names the
/// machine and its answer.
#[tokio::test]
async fn an_instance_nobody_will_take_says_why_on_its_row() {
    let a = mesh_testkit::spawn_founder_with("lonely", "why", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 0;
    })
    .await;
    web_service(&a);
    let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n";
    publish(
        &a,
        mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap(),
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(30), || running_by_node(&a, 2).len()
            == 1)
        .await,
        "the first instance did not start"
    );
    let mut reason = String::new();
    for _ in 0..60 {
        let view = status_of(&a, "web").await;
        if let Some(row) = view
            .instances
            .iter()
            .find(|i| i.state == "absent" && !i.health_detail.is_empty())
        {
            reason = row.health_detail.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        reason.starts_with("not placed") && reason.contains("lonely refused it"),
        "the absent slot does not say why: {reason:?}"
    );
    a.node.shutdown();
}

/// **Phase C, E8.** An instance goes where its affinity points, whichever way
/// the hash would have gone — and `jobs explain` answers for it, from the
/// node that placed it, with the deployment's own term in the breakdown.
/// Before Phase C nothing was compared to place an instance, so there was
/// nothing to explain.
#[tokio::test]
async fn an_affinity_places_an_instance_and_explain_says_why() {
    let a = spawn_founder("plain", "affinity").await;
    let b = mesh_testkit::join_new_node("fast", &a, vec!["ssd".into()]).await;
    let id_b = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(30), || a.node.engine().read(|s| s
            .members
            .get(&id_b)
            .is_some_and(|m| m.info.tags.iter().any(|t| t == "ssd"))))
        .await,
        "fast did not join with its tag"
    );
    web_service(&a);
    let src = concat!(
        "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\n\n",
        "[[deployment.affinity]]\nselect = \"tag=ssd\"\nweight = 100\n"
    );
    publish(
        &a,
        mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap(),
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(90), || running_by_node(&a, 1)
            == vec![id_b])
        .await,
        "the instance is not on the machine its affinity selects: running on {:?}",
        running_by_node(&a, 1)
    );

    // The reconciler is elected by name, so either node may be the one that
    // placed it and holds the explanation.
    let job = instance_id("web", 0, &Slot::Replica(0), 0);
    let mut placement = None;
    for _ in 0..120 {
        for node in [&a, &b] {
            if let IpcResponse::JobExplain(view) = mesh_testkit::ipc_as(
                node,
                founder_user_key(&a),
                IpcRequest::JobExplain { job_id: job },
            )
            .await
                && let Some(found) = view.placement
            {
                placement = Some(found);
            }
        }
        if placement.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Phase C, U8: the placement map, from either node, draws it there too.
    let IpcResponse::PlacementMap(map) =
        mesh_testkit::ipc_as(&a, founder_user_key(&a), IpcRequest::PlacementMap).await
    else {
        panic!("no placement map");
    };
    let on_b = map
        .iter()
        .find(|r| r.node == id_b)
        .expect("fast is on the map");
    assert_eq!(
        on_b.instances
            .iter()
            .map(|i| (i.deployment.as_str(), i.slot.as_str()))
            .collect::<Vec<_>>(),
        [("web", "0")],
        "{map:?}"
    );
    assert!(on_b.eligible && !on_b.draining);

    let placement = placement.expect("no node explains the instance it placed");
    assert_eq!(placement.chosen, Some(id_b));
    assert_eq!(placement.considered, 2, "{placement:?}");
    assert!(
        placement
            .candidates
            .iter()
            .all(|c| c.terms.iter().any(|t| t.name == "affinity")),
        "{placement:?}"
    );
    b.node.shutdown();
    a.node.shutdown();
}

/// **Phase C, E9.** An admin drains the machine an instance runs on, and the
/// instance moves without a gap: its replacement is already running on the
/// other machine at the moment the original stops. Before E9 a drain
/// cancelled the instance first and started its replacement after.
///
/// The drain is the one `meshnode nodes drain` authors — two tags assigned
/// in the mesh — so the drained node stays up throughout, as Nomad's does.
#[tokio::test]
async fn a_drained_node_hands_its_instance_over_before_stopping_it() {
    let a = spawn_founder("stays", "handover").await;
    let b = mesh_testkit::join_new_node("drained", &a, vec!["ssd".into()]).await;
    let (id_a, id_b) = (a.node.node_id(), b.node.node_id());
    assert!(
        wait_until(Duration::from_secs(30), || a.node.engine().read(|s| s
            .members
            .get(&id_b)
            .is_some_and(|m| m.info.tags.iter().any(|t| t == "ssd"))))
        .await,
        "drained did not join with its tag"
    );
    web_service(&a);
    let src = concat!(
        "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\n\n",
        "[[deployment.affinity]]\nselect = \"tag=ssd\"\nweight = 100\n\n",
        "[deployment.migrate]\nmin_healthy_time = \"3s\"\nhealthy_deadline = \"2m\"\n"
    );
    publish(
        &a,
        mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap(),
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(90), || running_by_node(&a, 1)
            == vec![id_b])
        .await,
        "the instance did not start on drained: {:?}",
        running_by_node(&a, 1)
    );

    let key = founder_user_key(&a);
    let mut assignment = mesh_proto::NodeTags {
        node_id: id_b,
        tags: mesh_proto::drain_tags(Some(mesh_proto::now_micros() + 600_000_000), false),
        by: key.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    assignment.signature = key.sign(&assignment.signing_bytes()).to_vec();
    let resp = mesh_testkit::ipc_as(&a, key, IpcRequest::NodeTags { assignment }).await;
    assert!(matches!(resp, IpcResponse::Ok), "drain failed: {resp:?}");

    // Watch for the moment the original stops, and look at the replacement
    // then.
    let mut replacement_was_running = None;
    for _ in 0..480 {
        let slot = incarnations(&a, "web", 0);
        if slot
            .iter()
            .any(|(_, st, ex)| *ex == id_b && st.is_terminal())
        {
            replacement_was_running = Some(
                slot.iter()
                    .any(|(_, st, ex)| *ex == id_a && matches!(st, mesh_proto::JobState::Running)),
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        replacement_was_running,
        Some(true),
        "the original must stop only once its replacement runs: {:?}",
        incarnations(&a, "web", 0)
    );
    assert!(
        b.node.engine().read(|s| s.members.contains_key(&id_a)),
        "the drained node is still a member and still up"
    );
    b.node.shutdown();
    a.node.shutdown();
}

/// **Phase C, E10.** A machine full of a job somebody marked `--preemptible`:
/// a deployment that names no priority waits for the slot, and one at
/// priority 80 takes it — the job is preempted under its owner's quota rules,
/// the instance runs, and the audit log says which deployment made room.
#[tokio::test]
async fn a_deployment_of_higher_priority_takes_a_preemptible_jobs_place() {
    let a = mesh_testkit::spawn_founder_with("full", "preempt", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 0;
    })
    .await;
    web_service(&a);
    let key = founder_user_key(&a);
    let batch = mesh_testkit::stream_as(
        &a,
        key,
        IpcRequest::Run {
            command: "sleep 300".into(),
            on: None,
            env: vec![(mesh_proto::REQ_PREEMPTIBLE.to_string(), "1".to_string())],
            timeout_secs: Some(600),
            fanout: None,
            priority: mesh_proto::JobPriority::Low,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(30), || !a
            .node
            .jobs_for_test()
            .preemptible_running()
            .is_empty())
        .await,
        "the preemptible job did not start"
    );
    let victim = a.node.jobs_for_test().preemptible_running()[0].0;

    publish(
        &a,
        mesh_core::deployments::parse(
            "[deployment]\nname = \"patient\"\nservice = \"web@1\"\ncount = 1\n",
            key.user_id(),
        )
        .unwrap(),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(
        a.node.jobs_for_test().is_running(&victim),
        "a deployment that names no priority preempted somebody's job"
    );

    publish(
        &a,
        mesh_core::deployments::parse(
            "[deployment]\nname = \"urgent\"\nservice = \"web@1\"\ncount = 1\npriority = 80\n",
            key.user_id(),
        )
        .unwrap(),
    )
    .await;
    let urgent = instance_id("urgent", 0, &Slot::Replica(0), 0);
    assert!(
        wait_until(Duration::from_secs(90), || a.node.engine().read(|s| s
            .jobs
            .get(&urgent)
            .is_some_and(|j| matches!(j.state, mesh_proto::JobState::Running))))
        .await,
        "the priority-80 instance never took the slot"
    );
    assert!(!a.node.jobs_for_test().is_running(&victim));
    assert!(
        a.node.engine().read(|s| s
            .audit
            .values()
            .any(|e| e.action == "preemption" && e.detail.contains("urgent"))),
        "no audit row names the deployment that made room"
    );
    drop(batch);
    a.node.shutdown();
}
