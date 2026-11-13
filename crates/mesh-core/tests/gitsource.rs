//! M5 Git sources (MASTERPLAN §8.1): a registered repo is ingested as a
//! content-addressed bundle and tracked services auto-bump onto each new
//! commit. Git is a source, never a runtime dependency.

use std::path::Path;
use std::process::Command;

use mesh_core::{gitsync, services};
use mesh_proto::{IpcRequest, IpcResponse, JobState, Runtime};
use mesh_testkit::{RunOutcome, founder_user_key, ipc_as, run_to_end, spawn_founder};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_all(dir: &Path, message: &str) {
    git_in(dir, &["add", "-A"]);
    git_in(dir, &["commit", "-m", message]);
}

#[tokio::test]
async fn git_source_ingests_and_auto_bumps_services() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let a = spawn_founder("alpha", "hyacinth").await;
    let user = founder_user_key(&a);

    // A local repo standing in for the upstream.
    let repo = tempfile::tempdir().expect("repo dir");
    git_in(repo.path(), &["init", "-b", "main"]);
    git_in(repo.path(), &["config", "user.email", "test@meshware"]);
    git_in(repo.path(), &["config", "user.name", "meshware test"]);
    std::fs::write(repo.path().join("hello.sh"), "echo version-one\n").expect("write script");
    commit_all(repo.path(), "v1");

    // Register the source with an auto-bump for the (future) service.
    let source = gitsync::build_source(
        user,
        "scripts".into(),
        repo.path().to_string_lossy().into_owned(),
        "main".into(),
        3600,
        vec!["hello".into()],
    );
    let resp = ipc_as(&a, user, IpcRequest::SourceAdd { source }).await;
    assert!(matches!(resp, IpcResponse::Ok), "source add: {resp:?}");

    // First sync ingests the tree as a bundle; no service exists yet.
    let resp = ipc_as(
        &a,
        user,
        IpcRequest::SourceSyncNow {
            name: "scripts".into(),
        },
    )
    .await;
    let IpcResponse::SourceSync {
        commit: c1,
        bundle_id: b1,
        unchanged,
        bumped,
    } = resp
    else {
        panic!("sync: {resp:?}");
    };
    assert!(!unchanged);
    assert!(bumped.is_empty(), "nothing to bump yet");
    assert!(
        a.node.engine().read(|s| s.bundles.contains_key(&b1)),
        "bundle must be announced"
    );

    // Bind a service to the ingested bundle.
    services::create_service_as(
        a.node.engine(),
        user,
        "hello".into(),
        Runtime::Bash,
        b1,
        String::new(),
        "hello.sh".into(),
        None,
        vec![],
        "git-tracked demo".into(),
        vec![],
    )
    .expect("create service");

    // A second sync with no new commit is a no-op.
    let resp = ipc_as(
        &a,
        user,
        IpcRequest::SourceSyncNow {
            name: "scripts".into(),
        },
    )
    .await;
    assert!(
        matches!(
            resp,
            IpcResponse::SourceSync {
                unchanged: true,
                ..
            }
        ),
        "unchanged sync: {resp:?}"
    );

    // New commit → new bundle → tracked service auto-bumps to v2.
    std::fs::write(repo.path().join("hello.sh"), "echo version-two\n").expect("update script");
    commit_all(repo.path(), "v2");
    let resp = ipc_as(
        &a,
        user,
        IpcRequest::SourceSyncNow {
            name: "scripts".into(),
        },
    )
    .await;
    let IpcResponse::SourceSync {
        commit: c2,
        bundle_id: b2,
        unchanged,
        bumped,
    } = resp
    else {
        panic!("second sync: {resp:?}");
    };
    assert!(!unchanged);
    assert_ne!(c1, c2);
    assert_ne!(b1, b2, "changed tree must produce a new bundle");
    assert_eq!(bumped, vec![("hello".to_string(), 2)]);

    // Replicated state: head moved to the source-authorized version.
    a.node.engine().read(|s| {
        let def = s.service_current("hello").expect("live service");
        assert_eq!(def.version, 2);
        assert_eq!(def.bundle_id, b2);
        assert_eq!(def.source.as_deref(), Some("scripts"));
        assert!(
            def.signature.is_empty(),
            "auto-bumped versions are source-authorized, not user-signed"
        );
        let last = s.sources["scripts"].last.as_ref().expect("sync status");
        assert_eq!(last.commit, c2);
        assert_eq!(last.bundle_id, b2);
    });

    // And the service actually runs the new content.
    match run_to_end(
        &a,
        user,
        IpcRequest::RunService {
            name: "hello".into(),
            on: None,
            inputs: vec![],
            inventory: None,
            timeout_secs: Some(30),
            fanout: None,
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await
    {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("version-two"), "stdout: {stdout:?}");
        }
        other => panic!("service run failed: {other:?}"),
    }

    a.node.shutdown();
}

/// **A plain transport is the node operator's call** (SEC-21). `http://`
/// and `git://` authenticate nobody, and what a watcher fetches is rolled
/// out to every service the source bumps.
#[tokio::test]
async fn a_plain_transport_needs_the_nodes_consent() {
    let strict = spawn_founder("alpha", "git-plain").await;
    let user = founder_user_key(&strict);
    for url in ["http://127.0.0.1:1/repo.git", "git://127.0.0.1:1/repo.git"] {
        let source = gitsync::build_source(
            user,
            "plain".into(),
            url.into(),
            "main".into(),
            3600,
            vec![],
        );
        match ipc_as(&strict, user, IpcRequest::SourceAdd { source }).await {
            IpcResponse::Error { message } => {
                assert!(message.contains("allow_plain"), "{message}")
            }
            other => panic!("{url} was accepted: {other:?}"),
        }
    }
    strict.node.shutdown();

    let lenient = mesh_testkit::spawn_founder_with("alpha", "git-plain-ok", vec![], |c| {
        c.git.allow_plain = true;
    })
    .await;
    let user = founder_user_key(&lenient);
    let source = gitsync::build_source(
        user,
        "plain".into(),
        "http://127.0.0.1:1/repo.git".into(),
        "main".into(),
        3600,
        vec![],
    );
    let resp = ipc_as(&lenient, user, IpcRequest::SourceAdd { source }).await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");
    lenient.node.shutdown();
}

/// **With `[git] require_signed`, an unsigned commit is never ingested.**
#[tokio::test]
async fn an_unsigned_commit_is_refused_when_signatures_are_required() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let a = mesh_testkit::spawn_founder_with("alpha", "git-signed", vec![], |c| {
        c.git.require_signed = true;
    })
    .await;
    let user = founder_user_key(&a);
    let repo = tempfile::tempdir().expect("repo dir");
    git_in(repo.path(), &["init", "-b", "main"]);
    git_in(repo.path(), &["config", "user.email", "test@meshware"]);
    git_in(repo.path(), &["config", "user.name", "meshware test"]);
    git_in(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("hello.sh"), "echo unsigned\n").expect("write script");
    commit_all(repo.path(), "unsigned");

    let source = gitsync::build_source(
        user,
        "unsigned".into(),
        repo.path().to_string_lossy().into_owned(),
        "main".into(),
        3600,
        vec![],
    );
    let resp = ipc_as(&a, user, IpcRequest::SourceAdd { source }).await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");
    let resp = ipc_as(
        &a,
        user,
        IpcRequest::SourceSyncNow {
            name: "unsigned".into(),
        },
    )
    .await;
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("require_signed"), "{message}")
        }
        other => panic!("an unsigned commit was synced: {other:?}"),
    }
    a.node.engine().read(|s| {
        assert!(
            s.sources["unsigned"].last.is_none(),
            "a refused commit was recorded as synced"
        );
    });
    // And nothing from it reached the checkout.
    let checkout = a.config().data_dir.join("git-sources").join("unsigned");
    assert!(!checkout.join("hello.sh").exists());
    a.node.shutdown();
}
