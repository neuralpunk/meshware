//! M23 people and access: enrolment is the missing conveyance of a key from
//! a person to an admin, done in-band and verifiably.
//!
//! What these tests pin is mostly what enrolment must *not* do. A request
//! is a row in a queue; it confers nothing, it cannot be forged into
//! somebody else's name, it cannot be smuggled past the approval that
//! actually grants power, and it does not accumulate forever.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    EnrolmentDecline, EnrolmentRequest, EnrolmentState, Grant, GrantId, IpcRequest, IpcResponse,
    NodeId, Role, UserId, UserRecord,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn signed_request(user: &Keypair, from_node: NodeId, username: &str) -> EnrolmentRequest {
    let now = mesh_proto::now_micros();
    let mut request = EnrolmentRequest {
        user_id: user.user_id(),
        suggested_username: username.into(),
        from_node,
        nonce: [7u8; 16],
        created_at_micros: now,
        expires_at_micros: now + mesh_proto::enrol::DEFAULT_TTL_SECS * 1_000_000,
        signature: Vec::new(),
    };
    request.signature = user.sign(&request.signing_bytes()).to_vec();
    request
}

async fn submit(node: &TestNode, user: &Keypair, username: &str) -> IpcResponse {
    let request = signed_request(user, node.node.node_id(), username);
    ipc_as(
        node,
        user,
        IpcRequest::EnrolSubmit {
            request: Box::new(request),
        },
    )
    .await
}

async fn pending(node: &TestNode, admin: &Keypair) -> Vec<mesh_proto::EnrolmentRow> {
    match ipc_as(node, admin, IpcRequest::Enrolments).await {
        IpcResponse::Enrolments(rows) => rows,
        other => panic!("enrolments: {other:?}"),
    }
}

async fn status(node: &TestNode, user: &Keypair) -> EnrolmentState {
    match ipc_as(node, user, IpcRequest::EnrolStatus).await {
        IpcResponse::EnrolStatus { state, .. } => state,
        other => panic!("enrol status: {other:?}"),
    }
}

async fn whoami(node: &TestNode, user: &Keypair) -> mesh_proto::WhoAmIInfo {
    match ipc_as(node, user, IpcRequest::WhoAmI).await {
        IpcResponse::WhoAmI(info) => info,
        other => panic!("whoami: {other:?}"),
    }
}

/// Approve exactly the way an admin does: the existing `UserAdd` +
/// `GrantAdd` pair, unchanged.
async fn approve(node: &TestNode, admin: &Keypair, user_id: UserId, username: &str, role: Role) {
    let mut record = UserRecord {
        user_id,
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::UserAdd { record }).await;
    assert!(matches!(resp, IpcResponse::Ok), "user add: {resp:?}");

    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id,
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await;
    assert!(matches!(resp, IpcResponse::Ok), "grant add: {resp:?}");
}

// ---------------------------------------------------------------------------
// the path the whole milestone exists for
// ---------------------------------------------------------------------------

/// Mia installs meshnode, asks, an admin sees her in a queue with a code,
/// approves her, and her next command works. No Slack, no 52-character
/// string, no silence.
#[tokio::test]
async fn a_person_asks_and_an_admin_approves() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();

    // Before asking: signed, denied, and the surface says so rather than
    // looking broken.
    let before = whoami(&founder, &mia).await;
    assert!(!before.registered);
    assert_eq!(before.enrolment, EnrolmentState::None);
    assert_eq!(before.node_name, "hq");
    let denied = ipc_as(&founder, &mia, IpcRequest::Status).await;
    assert!(matches!(denied, IpcResponse::Denied { .. }), "{denied:?}");

    let resp = submit(&founder, &mia, "mia").await;
    assert!(matches!(resp, IpcResponse::Ok), "enrol submit: {resp:?}");
    assert_eq!(status(&founder, &mia).await, EnrolmentState::Pending);

    let rows = pending(&founder, admin).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].user_id, mia.user_id());
    assert_eq!(rows[0].suggested_username, "mia");
    assert_eq!(rows[0].from_node, "hq", "the queue names the machine");
    assert!(!rows[0].declined);

    // Both sides derive the same code from the key and the nonce; neither
    // transmits it. That is what makes it worth reading aloud.
    let mine = whoami(&founder, &mia).await;
    assert_eq!(mine.enrolment_code.as_deref(), Some(rows[0].code.as_str()));

    approve(&founder, admin, mia.user_id(), "mia", Role::Operator).await;

    assert_eq!(status(&founder, &mia).await, EnrolmentState::Enrolled);
    let after = whoami(&founder, &mia).await;
    assert!(after.registered);
    assert_eq!(after.username.as_deref(), Some("mia"));
    assert_eq!(after.roles, vec!["operator".to_string()]);
    let ok = ipc_as(&founder, &mia, IpcRequest::Status).await;
    assert!(matches!(ok, IpcResponse::Status(_)), "{ok:?}");

    // An approved person leaves the queue rather than sitting in it as a
    // row somebody has to remember to dismiss.
    assert!(pending(&founder, admin).await.is_empty());
}

/// The request replicates: Mia asks the node in front of her, and the
/// admin on a different node sees it without anyone forwarding anything.
#[tokio::test]
async fn a_request_reaches_an_admin_on_another_node() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let laptop = join_new_node("laptop-mia", &founder, vec![]).await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();

    let resp = submit(&laptop, &mia, "mia").await;
    assert!(matches!(resp, IpcResponse::Ok), "enrol submit: {resp:?}");

    let seen = wait_until(Duration::from_secs(20), || {
        founder
            .node
            .engine()
            .read(|s| s.enrolments.contains_key(&mia.user_id()))
    })
    .await;
    assert!(seen, "the request never gossiped to the admin's node");

    let rows = pending(&founder, admin).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].from_node, "laptop-mia",
        "an admin needs the machine name, not a node id"
    );
}

// ---------------------------------------------------------------------------
// what a request must not be able to do
// ---------------------------------------------------------------------------

/// A request grants nothing. Merging one changes no authorization anywhere,
/// which is the property that makes replicating a queue of them safe.
#[tokio::test]
async fn a_pending_request_confers_no_authority() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let mia = Keypair::generate().unwrap();

    assert!(matches!(
        submit(&founder, &mia, "mia").await,
        IpcResponse::Ok
    ));

    for req in [
        IpcRequest::Status,
        IpcRequest::Users,
        IpcRequest::Enrolments,
    ] {
        let resp = ipc_as(&founder, &mia, req).await;
        assert!(
            matches!(resp, IpcResponse::Denied { .. }),
            "a pending request must not open any door: {resp:?}"
        );
    }
    assert!(
        founder
            .node
            .engine()
            .read(|s| !s.users.contains_key(&mia.user_id())),
        "a request must never become a user by itself"
    );
}

/// A caller may enrol themselves and nobody else — otherwise an admin's
/// queue could be filled with requests in other people's names.
#[tokio::test]
async fn a_request_must_name_the_key_that_signed_it() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let mia = Keypair::generate().unwrap();
    let someone_else = Keypair::generate().unwrap();

    let forged = signed_request(&someone_else, founder.node.node_id(), "not-me");
    let resp = ipc_as(
        &founder,
        &mia,
        IpcRequest::EnrolSubmit {
            request: Box::new(forged),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Error { .. }), "{resp:?}");
    assert!(
        founder.node.engine().read(|s| s.enrolments.is_empty()),
        "nothing should have been queued"
    );
}

/// The origin node is the one piece of context an admin reads that the
/// requester cannot vouch for. A node may carry anyone's request; it may
/// not claim the request came from somewhere else.
#[tokio::test]
async fn a_request_cannot_claim_a_node_it_did_not_come_from() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let mia = Keypair::generate().unwrap();

    let lying = signed_request(&mia, NodeId([9u8; 32]), "mia");
    let resp = ipc_as(
        &founder,
        &mia,
        IpcRequest::EnrolSubmit {
            request: Box::new(lying),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Error { .. }), "{resp:?}");
}

/// The queue is not an org chart for anyone who asks: listing it is
/// admin-only, and the requester still learns their own state.
#[tokio::test]
async fn the_queue_is_admin_only_but_your_own_state_is_not() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let viewer = Keypair::generate().unwrap();
    approve(&founder, admin, viewer.user_id(), "vera", Role::Viewer).await;
    let mia = Keypair::generate().unwrap();
    assert!(matches!(
        submit(&founder, &mia, "mia").await,
        IpcResponse::Ok
    ));

    let resp = ipc_as(&founder, &viewer, IpcRequest::Enrolments).await;
    assert!(matches!(resp, IpcResponse::Denied { .. }), "{resp:?}");

    // Mia has no roles at all and still gets an answer about herself.
    assert_eq!(status(&founder, &mia).await, EnrolmentState::Pending);
}

// ---------------------------------------------------------------------------
// declining and expiry: why the queue stays readable
// ---------------------------------------------------------------------------

/// Declining writes a tombstone, so the request stops reappearing — and
/// the person is told, rather than left watching a spinner.
#[tokio::test]
async fn declining_is_a_tombstone_and_is_visible_to_the_asker() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();
    assert!(matches!(
        submit(&founder, &mia, "mia").await,
        IpcResponse::Ok
    ));

    let mut decline = EnrolmentDecline {
        user_id: mia.user_id(),
        reason: "not this quarter".into(),
        declined_by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    decline.signature = admin.sign(&decline.signing_bytes()).to_vec();
    let resp = ipc_as(
        &founder,
        admin,
        IpcRequest::EnrolDecline {
            decline: Box::new(decline),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "decline: {resp:?}");

    assert_eq!(status(&founder, &mia).await, EnrolmentState::Declined);
    let rows = pending(&founder, admin).await;
    assert_eq!(
        rows.len(),
        1,
        "the row stays, marked, rather than vanishing"
    );
    assert!(rows[0].declined);
    assert!(!founder.node.engine().read(|s| {
        s.enrolments
            .get(&mia.user_id())
            .unwrap()
            .is_pending_at(mesh_proto::now_micros())
    }));
}

/// A decline suppresses the ask it answered, not the person. Asking again
/// afterwards is asking again, and an admin should see it.
#[tokio::test]
async fn asking_again_after_a_decline_is_visible() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();
    assert!(matches!(
        submit(&founder, &mia, "mia").await,
        IpcResponse::Ok
    ));

    let mut decline = EnrolmentDecline {
        user_id: mia.user_id(),
        reason: String::new(),
        declined_by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    decline.signature = admin.sign(&decline.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(
            &founder,
            admin,
            IpcRequest::EnrolDecline {
                decline: Box::new(decline)
            }
        )
        .await,
        IpcResponse::Ok
    ));
    assert_eq!(status(&founder, &mia).await, EnrolmentState::Declined);

    // Ask again, later, with a fresh nonce — so the admin does not see a
    // code they have already dismissed.
    let mut again = signed_request(&mia, founder.node.node_id(), "mia");
    again.created_at_micros = mesh_proto::now_micros() + 1_000_000;
    again.expires_at_micros = again.created_at_micros + 600 * 1_000_000;
    again.nonce = [8u8; 16];
    again.signature = mia.sign(&again.signing_bytes()).to_vec();
    let first_code = again.code();
    let resp = ipc_as(
        &founder,
        &mia,
        IpcRequest::EnrolSubmit {
            request: Box::new(again),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");

    assert_eq!(status(&founder, &mia).await, EnrolmentState::Pending);
    let rows = pending(&founder, admin).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].declined);
    assert_eq!(rows[0].code, first_code, "a re-ask gets a fresh code");
}

/// A request nobody acted on disappears. A permanently growing queue of
/// stale requests is how an approval queue stops being read.
#[tokio::test]
async fn an_unanswered_request_expires_out_of_the_queue() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();

    let now = mesh_proto::now_micros();
    let mut request = signed_request(&mia, founder.node.node_id(), "mia");
    request.created_at_micros = now - 10_000_000;
    request.expires_at_micros = now - 1_000_000; // already over
    request.signature = mia.sign(&request.signing_bytes()).to_vec();
    // The daemon takes it (the record is well-formed and self-signed);
    // the merge path is what refuses to queue something already dead.
    let _ = ipc_as(
        &founder,
        &mia,
        IpcRequest::EnrolSubmit {
            request: Box::new(request),
        },
    )
    .await;

    assert!(
        pending(&founder, admin).await.is_empty(),
        "an expired ask must not be shown"
    );
    assert_ne!(status(&founder, &mia).await, EnrolmentState::Pending);
}

/// Someone already enrolled has nothing to ask for, and the door says so
/// rather than quietly queuing a row nobody can act on.
#[tokio::test]
async fn an_enrolled_user_cannot_queue_a_request() {
    let founder = spawn_founder("hq", "enrol-mesh").await;
    let admin = founder_user_key(&founder);
    let mia = Keypair::generate().unwrap();
    approve(&founder, admin, mia.user_id(), "mia", Role::Viewer).await;

    let resp = submit(&founder, &mia, "mia").await;
    assert!(matches!(resp, IpcResponse::Error { .. }), "{resp:?}");
    assert!(pending(&founder, admin).await.is_empty());
}
