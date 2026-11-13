//! **A secret goes where it is needed, and only its people use it** (SEC-7).
//!
//! `secret set` sealed every secret for every member, the heal loop carried
//! it to every node within seconds — no-execute nodes
//! included — and any user who could run a job anywhere could print it.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, JobPriority, JobState, Liveness, NodeId, Role,
    SecretScope, UserRecord,
};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, join_new_node, run_to_end, spawn_founder,
    wait_until,
};

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

async fn alive(from: &TestNode, nodes: &[&TestNode]) {
    for n in nodes {
        let id = n.node.node_id();
        assert!(
            wait_until(Duration::from_secs(20), || {
                from.node.engine().read(|s| s.is_active_member(&id))
                    && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
            })
            .await
        );
    }
}

async fn recipients(node: &TestNode, name: &str) -> Vec<NodeId> {
    match ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::SecretRecipients {
            name: name.into(),
            nodes: None,
        },
    )
    .await
    {
        IpcResponse::SecretRecipients(r) => r,
        other => panic!("SecretRecipients: {other:?}"),
    }
}

async fn set(node: &TestNode, name: &str, value: &[u8], to: &[NodeId]) {
    let admin = founder_user_key(node);
    let mut record = mesh_core::secrets::seal_secret(name, 1, value, to, admin.user_id()).unwrap();
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::SecretPublish { record }).await,
        IpcResponse::Ok
    ));
}

fn wrapped_for(node: &TestNode, name: &str) -> Vec<NodeId> {
    node.node.engine().read(|s| {
        s.secrets[name]
            .record
            .wraps
            .iter()
            .map(|w| w.node_id)
            .collect()
    })
}

async fn scope(node: &TestNode, name: &str, nodes: &str, users: Vec<mesh_proto::UserId>) {
    let admin = founder_user_key(node);
    let mut scope = SecretScope {
        name: name.into(),
        nodes: nodes.into(),
        users,
        roles: vec![],
        by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    scope.signature = admin.sign(&scope.signing_bytes()).to_vec();
    match ipc_as(
        node,
        admin,
        IpcRequest::SecretScopeSet {
            scope: Box::new(scope),
        },
    )
    .await
    {
        IpcResponse::Ok => {}
        other => panic!("SecretScopeSet: {other:?}"),
    }
}

/// Run `echo` of the secret on `on`, as `user`; the outcome, and its
/// output.
async fn use_secret(node: &TestNode, user: &Keypair, on: &str) -> (bool, String) {
    let req = IpcRequest::Run {
        command: "echo \"got:$VALUE\"".into(),
        on: Some(format!("name={on}")),
        env: vec![("VALUE".into(), "{{secret:db}}".into())],
        timeout_secs: Some(30),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match run_to_end(node, user, req).await {
        RunOutcome::Done { stdout, state, .. } => (state == JobState::Succeeded, stdout),
        RunOutcome::Error(e) | RunOutcome::Denied(e) => (false, e),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unscoped_secret_skips_no_execute_nodes() {
    let founder = spawn_founder("hq", "scope-default").await;
    let worker = join_new_node("worker", &founder, vec![]).await;
    let idle = join_new_node("idle", &founder, vec![mesh_proto::NO_EXECUTE_TAG.into()]).await;
    alive(&founder, &[&worker, &idle]).await;
    let (f, w) = (founder.node.node_id(), worker.node.node_id());

    let mut to = recipients(&founder, "db").await;
    to.sort();
    let mut want = vec![f, w];
    want.sort();
    assert_eq!(to, want, "the default set is the members that run work");

    // Sealed for the founder alone, then healed: the worker is added, the
    // machine that runs no work is not.
    set(&founder, "db", b"hunter2", &[f]).await;
    founder.node.rewrap_secrets();
    let wrapped = wrapped_for(&founder, "db");
    assert!(wrapped.contains(&w), "{wrapped:?}");
    assert!(!wrapped.contains(&idle.node.node_id()), "{wrapped:?}");

    idle.stop().await;
    worker.stop().await;
    founder.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scope_narrows_where_a_secret_goes_and_who_may_use_it() {
    let founder = spawn_founder("hq", "scope-narrow").await;
    let db = join_new_node("db-1", &founder, vec![]).await;
    let web = join_new_node("web-1", &founder, vec![]).await;
    alive(&founder, &[&db, &web]).await;
    alive(&db, &[&founder, &web]).await;
    let admin = founder_user_key(&founder);
    let dev = Keypair::generate().unwrap();
    add_user(&founder, &dev, "dana", Role::Developer).await;

    // For db-1 only, and for the admin only.
    scope(&founder, "db", "name=db-1", vec![admin.user_id()]).await;
    assert_eq!(recipients(&founder, "db").await, vec![db.node.node_id()]);
    set(&founder, "db", b"hunter2", &[db.node.node_id()]).await;
    assert!(
        wait_until(Duration::from_secs(20), || {
            db.node.engine().read(|s| {
                s.secrets.contains_key("db")
                    && s.secret_scopes.contains_key("db")
                    && s.users.contains_key(&dev.user_id())
            })
        })
        .await
    );
    founder.node.rewrap_secrets();
    db.node.rewrap_secrets();
    assert_eq!(wrapped_for(&founder, "db"), vec![db.node.node_id()]);

    // The admin, on db-1: used.
    // The value is redacted from the output, so the marker is the proof it
    // was resolved.
    let (ok, out) = use_secret(&founder, admin, "db-1").await;
    assert!(ok && out.contains("got:[secret:db]"), "{out}");
    // A developer, on db-1: refused, and nothing printed.
    let (ok, out) = use_secret(&founder, &dev, "db-1").await;
    assert!(!ok, "a developer outside the scope read it: {out}");
    assert!(!out.contains("hunter2") && !out.contains("got:"), "{out}");
    // The admin, on a node the scope leaves out: refused there.
    let (ok, out) = use_secret(&founder, admin, "web-1").await;
    assert!(!ok, "a node outside the scope used it: {out}");

    // Widening the scope reaches web-1 without anyone setting the value.
    scope(&founder, "db", "name=db-1|name=web-1", vec![]).await;
    assert!(
        wait_until(Duration::from_secs(20), || {
            db.node
                .engine()
                .read(|s| s.secret_scopes["db"].nodes.contains("web-1"))
        })
        .await
    );
    db.node.rewrap_secrets();
    assert!(
        wait_until(Duration::from_secs(20), || {
            web.node.engine().read(|s| {
                s.secrets.get("db").is_some_and(|e| {
                    e.record
                        .wraps
                        .iter()
                        .any(|w| w.node_id == web.node.node_id())
                })
            })
        })
        .await,
        "a widened scope did not reach web-1"
    );
    let (ok, out) = use_secret(&founder, &dev, "web-1").await;
    assert!(ok && out.contains("got:[secret:db]"), "{out}");

    web.stop().await;
    db.stop().await;
    founder.stop().await;
}
