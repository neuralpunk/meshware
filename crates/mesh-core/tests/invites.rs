//! **An invite can be taken back, and only one the mesh knows opens the
//! door** (SEC-17).
//!
//! Nothing produced `InviteRevoke`, so a leaked token worked until it
//! expired; and a node that had not heard of an invite admitted anyone
//! holding it, with no limit on uses. These tests revoke through the same
//! request the CLI and Director send, and join through a node the invite
//! has not reached yet.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_core::membership::{self, InviteParams};
use mesh_proto::{InviteId, IpcRequest, IpcResponse};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, test_config_for, wait_until,
};

fn mint(founder: &TestNode, via: &TestNode, uses: u32) -> mesh_proto::InviteToken {
    membership::create_invite(
        founder.node.engine(),
        founder_user_key(founder),
        InviteParams {
            ttl_secs: 600,
            uses,
            tags: vec![],
            bootstrap: vec![via.addr_string()],
        },
    )
    .unwrap()
}

/// Join a fresh node through `via` only. The directory is kept alive by
/// the caller for as long as the joined config matters.
async fn try_join(
    dir: &tempfile::TempDir,
    via: &TestNode,
    token: &mesh_proto::InviteToken,
    name: &str,
) -> Result<(), String> {
    let config = test_config_for(dir.path(), name, "ignored", vec![]);
    membership::join_mesh(&config, &token.encode(), &[via.addr_string()])
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn revoke_request(user: &Keypair, invite_id: InviteId) -> IpcRequest {
    let by = user.user_id();
    IpcRequest::InviteRevoke {
        invite_id,
        by,
        signature: user
            .sign(&mesh_proto::invite_revoke_signing_bytes(&invite_id, &by))
            .to_vec(),
    }
}

async fn invites(node: &TestNode) -> Vec<mesh_proto::InviteRow> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Invites).await {
        IpcResponse::Invites(rows) => rows,
        other => panic!("Invites: {other:?}"),
    }
}

#[tokio::test]
async fn a_revoked_invite_is_refused_by_every_node() {
    let founder = spawn_founder("NIM", "invite-revoke").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let token = mint(&founder, &beta, 5);
    let id = token.invite_id;
    assert!(
        wait_until(Duration::from_secs(20), || {
            beta.node
                .engine()
                .read(|s| s.invite_uses_left(&id) == Some(5))
        })
        .await
    );

    let rows = invites(&founder).await;
    let row = rows.iter().find(|r| r.invite_id == id).expect("listed");
    assert_eq!((row.used, row.uses, row.revoked), (0, 5, false));

    // Refused: signed by somebody else, badly signed, or not an invite.
    let admin = founder_user_key(&founder);
    let other = Keypair::generate().unwrap();
    let IpcRequest::InviteRevoke { signature, .. } = revoke_request(&other, id) else {
        unreachable!()
    };
    let forged = IpcRequest::InviteRevoke {
        invite_id: id,
        by: admin.user_id(),
        signature,
    };
    match ipc_as(&founder, admin, forged).await {
        IpcResponse::Error { message } => assert!(message.contains("signature"), "{message}"),
        other => panic!("a badly signed revoke was accepted: {other:?}"),
    }
    match ipc_as(&founder, admin, revoke_request(&other, id)).await {
        IpcResponse::Denied { .. } => {}
        other => panic!("a revoke signed by another user was accepted: {other:?}"),
    }
    let unknown = InviteId::generate();
    match ipc_as(&founder, admin, revoke_request(admin, unknown)).await {
        IpcResponse::Error { message } => assert!(message.contains("no invite"), "{message}"),
        other => panic!("an unknown invite was revoked: {other:?}"),
    }
    assert!(!invites(&founder).await.iter().any(|r| r.revoked));

    match ipc_as(&founder, admin, revoke_request(admin, id)).await {
        IpcResponse::Ok => {}
        other => panic!("revoke: {other:?}"),
    }
    assert!(
        invites(&founder)
            .await
            .iter()
            .any(|r| r.invite_id == id && r.revoked)
    );
    // Again is fine, and changes nothing.
    assert!(matches!(
        ipc_as(&founder, admin, revoke_request(admin, id)).await,
        IpcResponse::Ok
    ));

    // The revoke reaches the other node, and the token stops working there.
    assert!(
        wait_until(Duration::from_secs(20), || {
            beta.node
                .engine()
                .read(|s| s.invite_uses_left(&id) == Some(0))
        })
        .await
    );
    let dir = tempfile::tempdir().unwrap();
    let err = try_join(&dir, &beta, &token, "latecomer")
        .await
        .expect_err("a revoked invite admitted a node");
    assert!(err.contains("revoked"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    let err = try_join(&dir, &founder, &token, "latecomer")
        .await
        .expect_err("a revoked invite admitted a node");
    assert!(err.contains("revoked"), "{err}");

    beta.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn a_node_that_has_not_heard_of_an_invite_asks_before_answering() {
    let founder = spawn_founder("NIM", "invite-learn").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let (f, b) = (founder.node.node_id(), beta.node.node_id());
    assert!(
        wait_until(Duration::from_secs(20), || {
            beta.node
                .engine()
                .read(|s| s.is_active_member(&f) && s.is_active_member(&b))
                && matches!(
                    beta.node.gossip().liveness_of(&f),
                    mesh_proto::Liveness::Alive
                )
        })
        .await
    );

    // Minted on the founder and presented to beta at once: gossip runs on
    // a one-second tick, so beta has usually not heard of it yet. Joining
    // is what has to make that invisible — and a few rounds make sure the
    // path is actually taken.
    let mut unheard = 0;
    let mut dirs = Vec::new();
    for round in 0..4 {
        let token = mint(&founder, &beta, 1);
        let id = token.invite_id;
        if beta.node.engine().read(|s| !s.invites.contains_key(&id)) {
            unheard += 1;
        }
        let dir = tempfile::tempdir().unwrap();
        try_join(&dir, &beta, &token, &format!("joiner-{round}"))
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        dirs.push(dir);
        // Spent: the use was recorded where it was taken.
        assert_eq!(
            beta.node.engine().read(|s| s.invite_uses_left(&id)),
            Some(0)
        );
        let dir = tempfile::tempdir().unwrap();
        let err = try_join(&dir, &beta, &token, "second")
            .await
            .expect_err("a single-use invite admitted two nodes");
        assert!(err.contains("no uses left"), "{err}");
    }
    assert!(
        unheard > 0,
        "every invite reached beta first; nothing was tested"
    );

    // An invite that was never published is refused, however well signed.
    let (unpublished, _) = membership::build_invite(
        founder.node.engine().genesis().body.mesh_id,
        founder_user_key(&founder),
        &InviteParams {
            ttl_secs: 600,
            uses: 50,
            tags: vec![],
            bootstrap: vec![beta.addr_string()],
        },
        f,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = try_join(&dir, &beta, &unpublished, "stowaway")
        .await
        .expect_err("an unpublished invite admitted a node");
    assert!(err.contains("no record of that invite"), "{err}");

    beta.stop().await;
    founder.stop().await;
}
