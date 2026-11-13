//! **Whose fleet is it?** — the two halves of that question, which the
//! product previously answered only by accident.
//!
//! 1. *Which members are machines.* Director is a peer of its own (D6), so
//!    a laptop that already runs `meshnode` and then opens Director puts
//!    two members in the mesh describing one box: same hostname, same
//!    cores, two rows in every fleet list, two entries in a version
//!    rollout, two "needs upgrading" badges — and the second one announces
//!    Director's *bundled* build, so it can be "outdated" while the machine
//!    is perfectly current. `no-execute` kept work away from it and did
//!    nothing about any of that, because it is a scheduling tag and this is
//!    a census problem. `console` is the census answer.
//!
//! 2. *Who decides what runs where.* A node announces `no-execute` about
//!    itself on its own `MemberUpdate`, and rewrites that record every time
//!    it boots — so it is the machine's statement about its own cores, and
//!    a machine can always take it back. `OpBody::NodeExecutionSet` is the
//!    other direction: an admin-signed record the subject cannot author,
//!    which is what "the owner controls the fleet" has to mean when the
//!    machine belongs to somebody else.

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, Liveness, NodeExecution};

/// Wait until the founder considers `peer` a live, addressable member —
/// placement filters on liveness, so a test that asks before the first
/// gossip round is asking the wrong question.
async fn wait_live(founder: &mesh_testkit::TestNode, peer: &mesh_testkit::TestNode) {
    let engine = founder.node.engine().clone();
    let gossip = founder.node.gossip().clone();
    let id = peer.node.node_id();
    let addr = peer.addr_string();
    let ok = mesh_testkit::wait_until(Duration::from_secs(15), || {
        engine.read(|s| s.is_active_member(&id) && s.members[&id].info.addrs.contains(&addr))
            && matches!(gossip.liveness_of(&id), Liveness::Alive)
    })
    .await;
    assert!(ok, "the joined node never became a live member");
}

async fn nodes(node: &mesh_testkit::TestNode, user: &Keypair) -> Vec<mesh_proto::NodeRow> {
    match mesh_testkit::ipc_as(node, user, IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("{other:?}"),
    }
}

async fn preview(
    node: &mesh_testkit::TestNode,
    user: &Keypair,
    selector: &str,
) -> Vec<mesh_proto::NodeRow> {
    match mesh_testkit::ipc_as(
        node,
        user,
        IpcRequest::TargetPreview {
            selector: selector.to_string(),
        },
    )
    .await
    {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("{other:?}"),
    }
}

/// Sign a contribute decision the way a client does.
fn decide(admin: &Keypair, node_id: mesh_proto::NodeId, allowed: bool, why: &str) -> NodeExecution {
    let mut d = NodeExecution {
        node_id,
        allowed,
        decided_by: admin.user_id(),
        decided_at_micros: mesh_proto::now_micros(),
        reason: why.to_string(),
        signature: Vec::new(),
    };
    d.signature = admin.sign(&d.signing_bytes()).to_vec();
    d
}

/// A revoked node leaves the census — including its denominator.
///
/// `members_alive` has always excluded revoked members and `members_total`
/// has not, so retiring a node moved the reading from "3 / 3" to "2 / 3"
/// and left it there for ever. Revocation is *the* remedy an operator
/// reaches for when a count looks wrong, so a count revocation cannot fix
/// is the worst possible shape for this bug.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_node_leaves_both_halves_of_the_count() {
    let founder = mesh_testkit::spawn_founder("nim", "fleet-mesh").await;
    let admin = mesh_testkit::founder_user_key(&founder).clone();
    let doomed = mesh_testkit::join_new_node("old-box", &founder, vec![]).await;
    wait_live(&founder, &doomed).await;

    let IpcResponse::Status(before) =
        mesh_testkit::ipc_as(&founder, &admin, IpcRequest::Status).await
    else {
        panic!("status")
    };
    assert_eq!((before.members_alive, before.members_total), (2, 2));

    let mut revocation = mesh_proto::NodeRevocation {
        node_id: doomed.node.node_id(),
        reason: "re-imaging the box".into(),
        by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = admin.sign(&revocation.signing_bytes()).to_vec();
    match mesh_testkit::ipc_as(&founder, &admin, IpcRequest::NodeRevoke { revocation }).await {
        IpcResponse::Ok => {}
        other => panic!("revoke refused: {other:?}"),
    }

    let IpcResponse::Status(after) =
        mesh_testkit::ipc_as(&founder, &admin, IpcRequest::Status).await
    else {
        panic!("status")
    };
    assert_eq!(
        (after.members_alive, after.members_total),
        (1, 1),
        "a revoked node stayed in the denominator"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_decides_which_machines_may_run_work() {
    let founder = mesh_testkit::spawn_founder("nim", "fleet-mesh").await;
    let admin = mesh_testkit::founder_user_key(&founder).clone();
    let joiner = mesh_testkit::join_new_node("stranger-box", &founder, vec![]).await;
    let joiner_id = joiner.node.node_id();
    wait_live(&founder, &joiner).await;

    // Nobody has decided: an existing mesh behaves exactly as it always did.
    assert!(
        preview(&founder, &admin, "any")
            .await
            .iter()
            .any(|r| r.node_id == joiner_id),
        "a node nobody has decided about must be a candidate"
    );

    // The owner says no.
    let resp = mesh_testkit::ipc_as(
        &founder,
        &admin,
        IpcRequest::NodeContributeSet {
            decision: Box::new(decide(&admin, joiner_id, false, "not my hardware")),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");

    let rows = nodes(&founder, &admin).await;
    let row = rows
        .iter()
        .find(|r| r.node_id == joiner_id)
        .expect("still a member");
    assert!(!row.contributes, "the decision is not visible on the row");
    assert_eq!(row.contribute_reason, "not my hardware");
    assert!(!row.revoked && !row.left, "it is still a full member");

    assert!(
        !preview(&founder, &admin, "any")
            .await
            .iter()
            .any(|r| r.node_id == joiner_id),
        "a vetoed node was still offered as a candidate"
    );

    // …and it is still refused when named directly, which is the case a
    // placement filter alone would miss: a re-dispatch of work planned
    // before the decision, or an origin running an older build.
    let outcome = mesh_testkit::run_to_end(
        &founder,
        &admin,
        IpcRequest::Run {
            command: "true".into(),
            on: Some("name=stranger-box".into()),
            env: Vec::new(),
            timeout_secs: Some(10),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(
        !matches!(outcome, mesh_testkit::RunOutcome::Done { .. }),
        "a vetoed node ran work it was named for: {outcome:?}"
    );

    // The owner changes their mind, and it is a candidate again.
    let resp = mesh_testkit::ipc_as(
        &founder,
        &admin,
        IpcRequest::NodeContributeSet {
            decision: Box::new(decide(&admin, joiner_id, true, "")),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");
    assert!(
        preview(&founder, &admin, "any")
            .await
            .iter()
            .any(|r| r.node_id == joiner_id),
        "the reversal did not take"
    );
}

/// The decision belongs to the mesh, not to the machine — so the machine
/// cannot sign one, and neither can a user who is merely *on* the mesh.
#[tokio::test(flavor = "multi_thread")]
async fn only_an_admin_can_decide() {
    let founder = mesh_testkit::spawn_founder("nim", "fleet-mesh").await;
    let admin = mesh_testkit::founder_user_key(&founder).clone();
    let joiner = mesh_testkit::join_new_node("stranger-box", &founder, vec![]).await;
    let joiner_id = joiner.node.node_id();
    wait_live(&founder, &joiner).await;

    let outsider = Keypair::generate().expect("key");
    let resp = mesh_testkit::ipc_as(
        &founder,
        &outsider,
        IpcRequest::NodeContributeSet {
            decision: Box::new(decide(&outsider, joiner_id, false, "mine now")),
        },
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Denied { .. }),
        "a stranger set the fleet's contribute policy: {resp:?}"
    );

    // A decision signed by somebody else, submitted by an admin, is also
    // refused: the signature and the sender have to be the same person, or
    // the audit trail names the wrong one.
    let resp = mesh_testkit::ipc_as(
        &founder,
        &admin,
        IpcRequest::NodeContributeSet {
            decision: Box::new(decide(&outsider, joiner_id, false, "mine now")),
        },
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Denied { .. }),
        "a decision signed by another key was accepted: {resp:?}"
    );

    assert!(
        preview(&founder, &admin, "any")
            .await
            .iter()
            .any(|r| r.node_id == joiner_id),
        "a refused decision took effect anyway"
    );
}

/// The whole reason this is an op and not a tag: a node re-announcing its
/// own record must not be able to lift the mesh's decision about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_cannot_clear_the_decision_by_re_announcing() {
    let founder = mesh_testkit::spawn_founder("nim", "fleet-mesh").await;
    let admin = mesh_testkit::founder_user_key(&founder).clone();
    let joiner = mesh_testkit::join_new_node("stranger-box", &founder, vec![]).await;
    let joiner_id = joiner.node.node_id();
    wait_live(&founder, &joiner).await;

    let resp = mesh_testkit::ipc_as(
        &founder,
        &admin,
        IpcRequest::NodeContributeSet {
            decision: Box::new(decide(&admin, joiner_id, false, "not my hardware")),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");

    // A restart re-announces the node's own `MemberUpdate` with whatever
    // tags it likes. That is the loophole a tag-based veto would have.
    let joiner = joiner.restart().await;
    assert_eq!(joiner.node.node_id(), joiner_id);

    assert!(
        !preview(&founder, &admin, "any")
            .await
            .iter()
            .any(|r| r.node_id == joiner_id),
        "the node announced its way out of the owner's decision"
    );
}
