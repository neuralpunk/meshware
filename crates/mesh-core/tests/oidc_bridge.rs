//! **The identity-provider bridge** (E1-P5), as replicated state.
//!
//! The token verifier itself is unit-tested next to the code it protects
//! (`mesh_core::oidc`), where it can sign real ES256 tokens with `ring` and
//! drive every attack — `alg: none`, algorithm confusion, a tampered
//! payload, a token bound to somebody else's key. This file is about the
//! other half: what the mesh *does* with a provider and an assertion.
//!
//! The property worth defending here is the one an enterprise buyer asks
//! about second, right after "do you support our IdP":
//!
//! > **What happens when the identity provider is down?**
//!
//! Nothing. Existing keys keep working, jobs keep running, and the only
//! thing that stops is new people enrolling *through SSO* — who can still
//! enrol the way they always could. That is not an accident of
//! configuration; nothing on the request path reads a provider record at
//! all, and [`the_mesh_does_not_consult_the_provider_to_do_its_job`]
//! asserts it.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    GroupRole, IpcRequest, IpcResponse, OidcAssertion, OidcProvider, OpBody, Role,
    oidc_provider_remove_signing_bytes,
};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until,
};

fn provider_for(admin: &Keypair, name: &str) -> OidcProvider {
    let mut p = OidcProvider {
        name: name.into(),
        issuer: "https://login.example.com/v2.0".into(),
        client_id: "meshware".into(),
        jwks_uri: "https://login.example.com/keys".into(),
        device_authorization_endpoint: "https://login.example.com/devicecode".into(),
        token_endpoint: "https://login.example.com/token".into(),
        scopes: vec!["openid".into()],
        groups_claim: "groups".into(),
        group_roles: vec![GroupRole {
            group: "platform".into(),
            role: Role::Operator,
        }],
        configured_by: admin.user_id(),
        configured_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    p.signature = admin.sign(&p.signing_bytes()).to_vec();
    p
}

async fn put(node: &TestNode, admin: &Keypair, p: OidcProvider) -> IpcResponse {
    ipc_as(node, admin, IpcRequest::OidcProviderPut { provider: p }).await
}

/// A provider is admin-signed replicated state, so every node ends up
/// believing the same issuer without any of them being authoritative.
#[tokio::test]
async fn a_provider_replicates_to_every_node() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("bravo", &a, vec![]).await;
    let admin = founder_user_key(&a).clone();

    assert!(matches!(
        put(&a, &admin, provider_for(&admin, "entra")).await,
        IpcResponse::Ok
    ));

    let seen = wait_until(Duration::from_secs(10), || {
        b.node
            .engine()
            .read(|s| s.oidc_providers.contains_key("entra"))
    })
    .await;
    assert!(seen, "bravo should learn the provider through gossip");

    b.node.engine().read(|s| {
        let (p, _) = s.oidc_providers.get("entra").unwrap();
        assert_eq!(p.issuer, "https://login.example.com/v2.0");
        assert_eq!(p.client_id, "meshware");
    });

    a.node.shutdown();
    b.node.shutdown();
}

/// Only an admin decides which issuer a mesh believes. A provider record
/// signed by somebody else is refused at the API — and if one is forged
/// onto the wire, the merge check on every node drops it too.
#[tokio::test]
async fn a_non_admin_cannot_name_an_issuer() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let nobody = Keypair::generate().unwrap();

    // Signed by a key the mesh has never heard of.
    let mut p = provider_for(&admin, "evil");
    p.configured_by = nobody.user_id();
    p.signature = nobody.sign(&p.signing_bytes()).to_vec();

    match ipc_as(&a, &nobody, IpcRequest::OidcProviderPut { provider: p }).await {
        IpcResponse::Denied { .. } | IpcResponse::Error { .. } => {}
        other => panic!("a stranger must not be able to configure an issuer: {other:?}"),
    }
    a.node.engine().read(|s| {
        assert!(!s.oidc_providers.contains_key("evil"));
    });
    a.node.shutdown();
}

/// Reconfiguring is last-writer-wins on one name — an admin who changes
/// their mind twice has said one thing.
#[tokio::test]
async fn reconfiguring_replaces_rather_than_accumulates() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();

    assert!(matches!(
        put(&a, &admin, provider_for(&admin, "entra")).await,
        IpcResponse::Ok
    ));
    let mut second = provider_for(&admin, "entra");
    second.client_id = "meshware-v2".into();
    second.configured_at_micros = mesh_proto::now_micros();
    second.signature = admin.sign(&second.signing_bytes()).to_vec();
    assert!(matches!(put(&a, &admin, second).await, IpcResponse::Ok));

    a.node.engine().read(|s| {
        assert_eq!(s.oidc_providers.len(), 1);
        assert_eq!(
            s.oidc_providers.get("entra").unwrap().0.client_id,
            "meshware-v2"
        );
    });
    a.node.shutdown();
}

/// Retiring a provider is its own signed statement, not a put with the
/// fields blanked: *"we no longer trust this issuer"* and *"its endpoints
/// moved"* are different things and an auditor should be able to tell them
/// apart.
#[tokio::test]
async fn a_provider_can_be_retired() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    assert!(matches!(
        put(&a, &admin, provider_for(&admin, "entra")).await,
        IpcResponse::Ok
    ));

    let by = admin.user_id();
    let at_micros = mesh_proto::now_micros();
    let signature = admin
        .sign(&oidc_provider_remove_signing_bytes("entra", &by, at_micros))
        .to_vec();
    assert!(matches!(
        ipc_as(
            &a,
            &admin,
            IpcRequest::OidcProviderRemove {
                name: "entra".into(),
                by,
                at_micros,
                signature,
            }
        )
        .await,
        IpcResponse::Ok
    ));
    a.node
        .engine()
        .read(|s| assert!(s.oidc_providers.is_empty()));
    a.node.shutdown();
}

/// **A node may only report its own verification.**
///
/// An assertion is the authoring node's claim about what it checked, and
/// the op envelope's node signature is its provenance. A node that authors
/// an assertion naming a *different* verifier is claiming credit for work
/// it did not do, and every peer drops it on merge.
#[tokio::test]
async fn a_node_cannot_claim_another_nodes_verification() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let subject = Keypair::generate().unwrap();

    let honest = OidcAssertion {
        user_id: subject.user_id(),
        verified_by: a.node.node_id(),
        provider: "entra".into(),
        issuer: "https://login.example.com/v2.0".into(),
        subject: "00u1abcd".into(),
        email: "mia@example.com".into(),
        display_name: "mia".into(),
        groups: vec!["platform".into()],
        suggested_role: Some(Role::Operator),
        verified_at_micros: mesh_proto::now_micros(),
        expires_at_micros: mesh_proto::now_micros() + 3_600_000_000,
    };
    a.node
        .engine()
        .author_op(OpBody::OidcAssertionPut {
            assertion: honest.clone(),
        })
        .expect("a node may report its own verification");
    a.node.engine().read(|s| {
        assert!(s.oidc_assertions.contains_key(&subject.user_id()));
    });

    // Now the same node claims somebody else did the verifying.
    let other = Keypair::generate().unwrap();
    let impostor = Keypair::generate().unwrap();
    let mut lie = honest.clone();
    lie.user_id = other.user_id();
    lie.verified_by = impostor.node_id();
    a.node
        .engine()
        .author_op(OpBody::OidcAssertionPut { assertion: lie })
        .expect("authoring succeeds; the merge rule is what refuses");

    a.node.engine().read(|s| {
        assert!(
            !s.oidc_assertions.contains_key(&other.user_id()),
            "an assertion naming a verifier that is not its author must be dropped"
        );
    });
    a.node.shutdown();
}

/// A verified login shows up where the decision is actually made: the
/// pending queue an admin reads. It is **evidence, not authority** — the
/// person is still not a user until an admin signs a `UserAdd`.
#[tokio::test]
async fn a_verified_identity_reaches_the_approval_queue_without_granting_anything() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();
    let mia = Keypair::generate().unwrap();

    // Mia asks to join, the way she always could.
    let mut request = mesh_proto::EnrolmentRequest {
        user_id: mia.user_id(),
        suggested_username: "mia".into(),
        from_node: a.node.node_id(),
        nonce: [4u8; 16],
        created_at_micros: mesh_proto::now_micros(),
        expires_at_micros: mesh_proto::now_micros() + 600_000_000,
        signature: Vec::new(),
    };
    request.signature = mia.sign(&request.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(
            &a,
            &mia,
            IpcRequest::EnrolSubmit {
                request: Box::new(request),
            }
        )
        .await,
        IpcResponse::Ok
    ));

    // …and a node vouches for what the provider proved.
    a.node
        .engine()
        .author_op(OpBody::OidcAssertionPut {
            assertion: OidcAssertion {
                user_id: mia.user_id(),
                verified_by: a.node.node_id(),
                provider: "entra".into(),
                issuer: "https://login.example.com/v2.0".into(),
                subject: "00u1abcd".into(),
                email: "mia@example.com".into(),
                display_name: "Mia".into(),
                groups: vec!["platform".into()],
                suggested_role: Some(Role::Operator),
                verified_at_micros: mesh_proto::now_micros(),
                expires_at_micros: mesh_proto::now_micros() + 3_600_000_000,
            },
        })
        .unwrap();

    let IpcResponse::Enrolments(rows) = ipc_as(&a, &admin, IpcRequest::Enrolments).await else {
        panic!("expected the pending queue");
    };
    let row = rows
        .iter()
        .find(|r| r.user_id == mia.user_id())
        .expect("mia should be in the queue");
    let v = row
        .verified
        .as_ref()
        .expect("the admin should see what the provider proved");
    assert_eq!(v.provider, "entra");
    assert_eq!(v.subject, "00u1abcd");
    assert_eq!(v.email, "mia@example.com");
    assert_eq!(v.groups, vec!["platform"]);
    assert_eq!(v.suggested_role.as_deref(), Some("operator"));
    assert!(v.fresh);

    // And none of it granted anything.
    let founder = a.node.engine().genesis().body.founder_user;
    a.node.engine().read(|s| {
        assert!(
            !s.users.contains_key(&mia.user_id()),
            "a verified login must not create a user — approval is still an admin's signature"
        );
        assert!(
            mesh_core::rbac::roles_at(s, &founder, &mia.user_id(), mesh_proto::now_micros())
                .is_empty(),
            "and it must not grant a role"
        );
    });
    a.node.shutdown();
}

/// **The fail-open property**, and the reason it holds by construction.
///
/// With a provider configured and completely unreachable — the endpoints
/// here resolve to nothing — the mesh schedules, executes and authorizes
/// exactly as it did before. Nothing on the request path reads a provider
/// record, so an IdP outage cannot become a mesh outage.
#[tokio::test]
async fn the_mesh_does_not_consult_the_provider_to_do_its_job() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a).clone();

    let mut p = provider_for(&admin, "entra");
    // A host that will never answer.
    p.issuer = "https://idp.invalid/v2.0".into();
    p.jwks_uri = "https://idp.invalid/keys".into();
    p.device_authorization_endpoint = "https://idp.invalid/devicecode".into();
    p.token_endpoint = "https://idp.invalid/token".into();
    p.signature = admin.sign(&p.signing_bytes()).to_vec();
    assert!(matches!(put(&a, &admin, p).await, IpcResponse::Ok));

    match mesh_testkit::run_to_end(
        &a,
        &admin,
        IpcRequest::Run {
            command: "echo mesh-unaffected".into(),
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
    {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, mesh_proto::JobState::Succeeded);
            assert!(stdout.contains("mesh-unaffected"));
        }
        other => panic!("an unreachable identity provider must not affect the mesh: {other:?}"),
    }
    a.node.shutdown();
}
