//! **Machine principals** (E1-P4): keys that belong to systems, not people.
//!
//! The problem this closes is one a security reviewer finds immediately:
//! before this, a CI pipeline authenticated as a *person*, with a copy of
//! that person's long-lived key file sitting on a runner. Every audit row
//! it produced was attributed to a human who was not there.
//!
//! The shape of the fix is worth stating because it is deliberately small.
//! A service account **is** a user — same Ed25519 key-is-identity, same
//! signed requests, same scoped `Grant` with its selector and glob — plus a
//! marker op saying the principal is a machine. That marker buys three
//! things a `UserRecord` alone cannot say: actor type (as a *projection*,
//! so no already-signed struct changed), an expiry on the principal itself
//! rather than only on its grants, and a creation flow that mints the key
//! so it was never a person's.
#![allow(clippy::unwrap_used)]

use mesh_core::identity::Keypair;
use mesh_core::rbac;
use mesh_proto::{ActorType, IpcRequest, IpcResponse, Role, ServiceAccount, UserRecord};
use mesh_testkit::{RunOutcome, TestNode, founder_user_key, ipc_as, spawn_founder};

/// Mint a machine principal the way the CLI does: `UserAdd`, then the
/// marker, then a scoped grant — three independently signed facts, in that
/// order, because the daemon's admin refusal has to see a machine when it
/// evaluates the grant.
async fn mint(
    node: &TestNode,
    admin: &Keypair,
    name: &str,
    role: Role,
    expires_at_micros: Option<u64>,
) -> Keypair {
    let kp = Keypair::generate().unwrap();
    let user_id = kp.user_id();

    let mut record = UserRecord {
        user_id,
        username: name.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));

    let mut account = ServiceAccount {
        user_id,
        name: name.into(),
        description: "test pipeline".into(),
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        expires_at_micros,
        signature: Vec::new(),
    };
    account.signature = admin.sign(&account.signing_bytes()).to_vec();
    match ipc_as(node, admin, IpcRequest::ServiceAccountAdd { account }).await {
        IpcResponse::Ok => {}
        other => panic!("service account add refused: {other:?}"),
    }

    grant(node, admin, user_id, role, expires_at_micros).await;
    kp
}

async fn grant(
    node: &TestNode,
    admin: &Keypair,
    user_id: mesh_proto::UserId,
    role: Role,
    expires_at_micros: Option<u64>,
) -> IpcResponse {
    let mut grant = mesh_proto::Grant {
        grant_id: mesh_proto::GrantId::generate(),
        user_id,
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await
}

async fn run_as(node: &TestNode, who: &Keypair, cmd: &str) -> RunOutcome {
    mesh_testkit::run_to_end(
        node,
        who,
        IpcRequest::Run {
            command: cmd.into(),
            on: None,
            env: vec![],
            timeout_secs: Some(30),
            fanout: None,
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await
}

fn founder_of(node: &TestNode) -> [u8; 32] {
    node.node.engine().genesis().body.founder_user
}

/// The point of the whole feature: a machine credential is a first-class
/// principal that can do its job.
#[tokio::test]
async fn a_service_account_can_run_work() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let ci = mint(&a, &admin, "ci-deploy", Role::Developer, None).await;

    match run_as(&a, &ci, "echo ran-as-a-machine").await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, mesh_proto::JobState::Succeeded);
            assert!(stdout.contains("ran-as-a-machine"), "stdout: {stdout}");
        }
        other => panic!("a granted service account should be able to run: {other:?}"),
    }
    a.node.shutdown();
}

/// Actor type is a **projection over state**, never a stored field — which
/// is what let this ship without touching `AuditEntry`, whose postcard
/// encoding is signed into every audit op ever written.
#[tokio::test]
async fn actor_type_is_derived_not_stored() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let ci = mint(&a, &admin, "ci-deploy", Role::Developer, None).await;

    a.node.engine().read(|s| {
        assert_eq!(rbac::actor_type(s, &ci.user_id()), ActorType::Machine);
        assert_eq!(rbac::actor_type(s, &admin.user_id()), ActorType::Person);
    });
    a.node.shutdown();
}

/// **The property the feature exists for.** An expired account cannot act,
/// and the two clocks are different: a lapsed *grant* leaves a live key
/// with no roles that an admin can re-grant; a lapsed *account* is terminal
/// for that credential.
#[tokio::test]
async fn an_expired_service_account_cannot_act_however_it_is_granted() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();

    // Already past its clock when it is created — no sleeping in a test.
    let past = mesh_proto::now_micros().saturating_sub(60 * 1_000_000);
    let ci = mint(&a, &admin, "ci-stale", Role::Developer, Some(past)).await;

    match run_as(&a, &ci, "echo should-not-run").await {
        RunOutcome::Denied(msg) => {
            assert!(
                msg.contains("expired"),
                "the denial must name the clock that ran out, not just refuse: {msg}"
            );
            assert!(msg.contains("ci-stale"), "and name the account: {msg}");
        }
        other => panic!("an expired service account must be refused: {other:?}"),
    }

    // Re-granting does nothing: the principal itself is out of time. This is
    // the distinction from a grant expiry, asserted rather than described.
    assert!(matches!(
        grant(&a, &admin, ci.user_id(), Role::Developer, None).await,
        IpcResponse::Ok
    ));
    assert!(
        matches!(
            run_as(&a, &ci, "echo still-no").await,
            RunOutcome::Denied(_)
        ),
        "a fresh unexpiring grant must not resurrect an expired principal"
    );

    let founder = founder_of(&a);
    a.node.engine().read(|s| {
        assert!(
            rbac::roles_at(s, &founder, &ci.user_id(), mesh_proto::now_micros()).is_empty(),
            "an expired principal holds no roles at the choke point every path uses"
        );
    });
    a.node.shutdown();
}

/// A service account may not hold admin — refused at the API, because an
/// admin key can mint users and grants and a leaked pipeline credential
/// holding one is permanent mesh compromise.
#[tokio::test]
async fn a_service_account_may_not_be_an_admin() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let ci = mint(&a, &admin, "ci-deploy", Role::Developer, None).await;

    match grant(&a, &admin, ci.user_id(), Role::Admin, None).await {
        IpcResponse::Error { message } => {
            assert!(
                message.contains("admin"),
                "the refusal should say why: {message}"
            );
        }
        other => panic!("admin on a service account must be refused: {other:?}"),
    }

    // And it really did not take effect.
    let founder = founder_of(&a);
    a.node.engine().read(|s| {
        assert!(
            !rbac::roles_at(s, &founder, &ci.user_id(), mesh_proto::now_micros())
                .contains(&Role::Admin)
        );
    });
    a.node.shutdown();
}

/// Revocation is the same verb it is for a person — one principal model,
/// not two.
#[tokio::test]
async fn a_service_account_revokes_like_a_person() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let ci = mint(&a, &admin, "ci-deploy", Role::Developer, None).await;

    let mut revocation = mesh_proto::UserRevocation {
        user_id: ci.user_id(),
        reason: "pipeline retired".into(),
        revoked_by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = admin.sign(&revocation.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&a, &admin, IpcRequest::UserRevoke { revocation }).await,
        IpcResponse::Ok
    ));

    let founder = founder_of(&a);
    a.node.engine().read(|s| {
        assert!(
            !rbac::is_principal_active(s, &founder, &ci.user_id(), mesh_proto::now_micros()),
            "a revoked service account is not an active principal"
        );
    });
    assert!(
        matches!(run_as(&a, &ci, "echo nope").await, RunOutcome::Denied(_)),
        "and it cannot run"
    );
    a.node.shutdown();
}

/// The marker describes a principal; it does not create one. Publishing it
/// for a key nobody added is refused at the API rather than parked on merge
/// for ever.
#[tokio::test]
async fn the_marker_requires_the_user_to_exist() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let orphan = Keypair::generate().unwrap();

    let mut account = ServiceAccount {
        user_id: orphan.user_id(),
        name: "ghost".into(),
        description: String::new(),
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        expires_at_micros: None,
        signature: Vec::new(),
    };
    account.signature = admin.sign(&account.signing_bytes()).to_vec();
    match ipc_as(&a, &admin, IpcRequest::ServiceAccountAdd { account }).await {
        IpcResponse::Error { message } => {
            assert!(message.contains("no user"), "message was: {message}");
        }
        other => panic!("a marker for an unknown key must be refused: {other:?}"),
    }
    a.node.shutdown();
}

/// A machine principal survives a restart, because it is replicated state
/// like every other governance artifact — not a token in memory somewhere.
#[tokio::test]
async fn a_service_account_survives_a_restart() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let ci = mint(&a, &admin, "ci-deploy", Role::Developer, None).await;
    let id = ci.user_id();

    let a = a.restart().await;
    let founder = founder_of(&a);
    a.node.engine().read(|s| {
        assert_eq!(rbac::actor_type(s, &id), ActorType::Machine);
        assert!(rbac::is_principal_active(
            s,
            &founder,
            &id,
            mesh_proto::now_micros()
        ));
    });
    a.node.shutdown();
}
