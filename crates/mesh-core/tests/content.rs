//! M2 exit criteria: push content, register a service with a contract, and
//! run it — including bundle replication to a remote executor.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, JobState, Liveness, Runtime};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, spawn_founder, wait_until,
};

/// Drive a service run as the founding admin, capturing either completion
/// or a pre-run error.
async fn run_service_raw(
    node: &TestNode,
    name: &str,
    on: Option<String>,
    inputs: Vec<(String, String)>,
    inventory: Option<String>,
) -> RunOutcome {
    let req = IpcRequest::RunService {
        name: name.to_string(),
        on,
        inputs,
        inventory,
        timeout_secs: Some(120),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    mesh_testkit::run_to_end(node, founder_user_key(node), req).await
}

/// Convenience: assert the run completed, returning (stdout, state, code).
async fn run_service(
    node: &TestNode,
    name: &str,
    on: Option<String>,
    inputs: Vec<(String, String)>,
    inventory: Option<String>,
) -> (String, JobState, Option<i32>) {
    match run_service_raw(node, name, on, inputs, inventory).await {
        RunOutcome::Done {
            stdout,
            state,
            code,
            ..
        } => (stdout, state, code),
        RunOutcome::Denied(e) => panic!("run denied: {e}"),
        RunOutcome::Error(e) => panic!("run error: {e}"),
    }
}

/// Create a bundle directory with the given files, returning its path.
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

#[tokio::test]
async fn push_and_run_local_bash_service() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[(
        "scripts/hello.sh",
        "#!/bin/bash\necho \"hello from bundle\"\n",
        0o755,
    )]);

    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    // The founder holds its own bundle.
    assert!(engine.read(|s| {
        s.bundle_holders(&push.bundle_id)
            .contains(&a.node.node_id())
    }));

    let version = mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "greet".into(),
        Runtime::Bash,
        push.bundle_id,
        "scripts".into(),
        "hello.sh".into(),
        None,
        vec![],
        "greeting service".into(),
        vec![],
    )
    .expect("create service");
    assert_eq!(version, 1);

    let (out, state, code) = run_service(&a, "greet", None, vec![], None).await;
    assert_eq!(state, JobState::Succeeded, "stdout: {out}");
    assert_eq!(code, Some(0));
    assert!(out.contains("hello from bundle"), "stdout: {out}");

    a.node.shutdown();
}

#[tokio::test]
async fn contract_rejects_bad_inputs() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let bundle = make_bundle_dir(&[("run.sh", "#!/bin/bash\necho ok\n", 0o755)]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).unwrap();

    let schema = r#"{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}"#;
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "needs-count".into(),
        Runtime::Bash,
        push.bundle_id,
        "".into(),
        "run.sh".into(),
        Some(schema.to_string()),
        vec![],
        String::new(),
        vec![],
    )
    .unwrap();

    // Missing required input → error before scheduling.
    match run_service_raw(&a, "needs-count", None, vec![], None).await {
        RunOutcome::Error(msg) => assert!(msg.contains("validation"), "unexpected error: {msg}"),
        other => panic!("missing required input should have been rejected: {other:?}"),
    }

    // Valid input → runs.
    let (out, state, code) = run_service(
        &a,
        "needs-count",
        None,
        vec![("count".into(), "5".into())],
        None,
    )
    .await;
    assert_eq!(state, JobState::Succeeded, "stdout: {out}");
    assert_eq!(code, Some(0));

    a.node.shutdown();
}

#[tokio::test]
async fn bundle_replicates_to_remote_executor() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("beta", &a, vec!["worker".into()]).await;

    // Wait until alpha sees beta alive.
    let engine_a = a.node.engine().clone();
    let gossip_a = a.node.gossip().clone();
    let beta_id = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_a.read(|s| s.is_active_member(&beta_id))
                && matches!(gossip_a.liveness_of(&beta_id), Liveness::Alive)
        })
        .await
    );

    // Push a python service on alpha only.
    let bundle = make_bundle_dir(&[(
        "app/main.py",
        "import sys\nprint('python says', ' '.join(sys.argv[1:]) or 'hi')\n",
        0o644,
    )]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).unwrap();
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "pytest-svc".into(),
        Runtime::Python,
        push.bundle_id,
        "app".into(),
        "main.py".into(),
        None,
        vec![],
        String::new(),
        vec![],
    )
    .unwrap();

    // Wait for beta to learn the service + bundle via gossip.
    let engine_b = b.node.engine().clone();
    let bid = push.bundle_id;
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_b
                .read(|s| s.service_current("pytest-svc").is_some() && s.bundles.contains_key(&bid))
        })
        .await
    );

    // Run it, forcing execution on beta by tag. Beta must fetch the bundle
    // from alpha (it was never pushed there), build a python env, and run.
    let (out, state, code) =
        run_service(&a, "pytest-svc", Some("tag=worker".into()), vec![], None).await;
    assert_eq!(state, JobState::Succeeded, "stdout: {out}");
    assert_eq!(code, Some(0));
    assert!(out.contains("python says"), "stdout: {out}");

    // Beta now holds the bundle it fetched.
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine_b.read(|s| s.bundle_holders(&bid).contains(&beta_id))
        })
        .await,
        "beta should record its hold after fetching"
    );

    a.node.shutdown();
    b.node.shutdown();
}
