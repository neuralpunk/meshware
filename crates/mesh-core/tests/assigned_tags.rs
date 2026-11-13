//! Tags an admin puts on **another** node, kept in the mesh.
//!
//! Before this, every tag was a node's statement about itself: `meshnode
//! nodes tag` edited the local node's `MemberUpdate` and nothing else. That
//! made the one thing tags are actually for — an operator's division of
//! their own estate, `--on tag=prod` — into an SSH loop, and it did not
//! survive a rebuild, because the machine that came back had never heard of
//! `prod`.
//!
//! The properties worth pinning are that an assignment reaches the whole
//! mesh, that it survives the subject re-announcing (which is the failure
//! that makes writing into `MemberUpdate` the wrong design), that the two
//! halves of a node's tag list do not clobber each other in either
//! direction, and that a tag only the machine can know is refused with a
//! reason rather than accepted as a lie.

use std::time::Duration;

use mesh_core::membership::Selector;
use mesh_proto::{IpcRequest, IpcResponse, Liveness, NodeId};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

/// Sign and send an assignment as the founder (the mesh's only admin here).
async fn assign(at: &TestNode, node_id: NodeId, tags: &[&str]) -> IpcResponse {
    let user = founder_user_key(at);
    let mut assignment = mesh_proto::NodeTags {
        node_id,
        tags: tags.iter().map(|t| (*t).to_string()).collect(),
        by: user.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    assignment.signature = user.sign(&assignment.signing_bytes()).to_vec();
    ipc_as(at, user, IpcRequest::NodeTags { assignment }).await
}

/// The merged tag list one node holds for another.
fn tags_of(at: &TestNode, node_id: NodeId) -> Vec<String> {
    at.node
        .engine()
        .read(|s| s.members.get(&node_id).map(|m| m.info.tags.clone()))
        .unwrap_or_default()
}

async fn has_tag(at: &TestNode, node_id: NodeId, tag: &str) -> bool {
    wait_until(Duration::from_secs(15), || {
        tags_of(at, node_id).iter().any(|t| t == tag)
    })
    .await
}

async fn converged(a: &TestNode, b: &TestNode) -> bool {
    wait_until(Duration::from_secs(15), || {
        a.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                a.node.gossip().liveness_of(&b.node.node_id()),
                Liveness::Alive
            )
    })
    .await
}

/// The whole point: an operator labels a machine they are not on, and every
/// node — including the subject — agrees, and `tag=` finds it.
#[tokio::test(flavor = "multi_thread")]
async fn an_admin_tags_a_node_they_are_not_on() {
    let a = spawn_founder("a", "tags-remote").await;
    let b = join_new_node("b", &a, vec!["built-in".into()]).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let b_id = b.node.node_id();

    assert!(
        matches!(
            assign(&a, b_id, &["prod", "dc=east"]).await,
            IpcResponse::Ok
        ),
        "the assignment was refused"
    );

    assert!(
        has_tag(&a, b_id, "prod").await,
        "not visible where it was set"
    );
    assert!(has_tag(&b, b_id, "prod").await, "never reached the subject");
    assert!(
        has_tag(&a, b_id, "dc=east").await,
        "second tag went missing"
    );

    // The node's own announcement is still there, beside it.
    assert!(
        tags_of(&a, b_id).iter().any(|t| t == "built-in"),
        "assigning clobbered what the node announced: {:?}",
        tags_of(&a, b_id)
    );

    // And the thing tags exist for resolves against it, from the node that
    // is not the subject.
    let hit = a.node.engine().read(|s| {
        mesh_core::membership::resolve_targets(
            s,
            &Selector::parse("tag=prod"),
            a.node.node_id(),
            |id| a.node.gossip().liveness_of(id),
        )
    });
    assert_eq!(
        hit.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        vec!["b"],
        "`--on tag=prod` did not find the node it was assigned to"
    );

    // The row separates the halves, so a surface can offer to remove only
    // what it is able to remove.
    let rows = match ipc_as(&a, founder_user_key(&a), IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("expected Nodes, got {other:?}"),
    };
    let row = rows.iter().find(|r| r.node_id == b_id).expect("b's row");
    assert!(row.assigned_tags.contains(&"prod".to_string()));
    assert!(
        !row.assigned_tags.contains(&"built-in".to_string()),
        "a node's own announcement was reported as assigned: {:?}",
        row.assigned_tags
    );
}

/// The failure that decides the design. A node re-announces its own tags on
/// every boot, so an assignment written *into* that announcement would be
/// erased by the next restart — silently, and days later.
#[tokio::test(flavor = "multi_thread")]
async fn an_assignment_survives_the_node_restarting() {
    let a = spawn_founder("a", "tags-restart").await;
    let b = join_new_node("b", &a, vec!["built-in".into()]).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let b_id = b.node.node_id();

    assert!(matches!(assign(&a, b_id, &["prod"]).await, IpcResponse::Ok));
    assert!(
        has_tag(&b, b_id, "prod").await,
        "never landed on the subject"
    );

    let b = b.restart().await;

    // The subject re-announced everything it knows about itself…
    assert!(
        wait_until(Duration::from_secs(20), || tags_of(&b, b_id)
            .iter()
            .any(|t| t == "built-in"))
        .await,
        "the node did not re-announce its own tags"
    );
    // …and the operator's label is still on it, on both nodes.
    assert!(
        has_tag(&b, b_id, "prod").await,
        "a restart erased the assignment: {:?}",
        tags_of(&b, b_id)
    );
    assert!(
        has_tag(&a, b_id, "prod").await,
        "the assignment was lost from the node that made it"
    );
}

/// The two halves are independent in both directions: unassigning leaves
/// the node's own announcement alone, and the node's own edits leave the
/// assignment alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_two_halves_do_not_clobber_each_other() {
    let a = spawn_founder("a", "tags-halves").await;
    let b = join_new_node("b", &a, vec!["shared".into()]).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let b_id = b.node.node_id();

    // Assign a tag the node already announces, plus one it does not.
    assert!(matches!(
        assign(&a, b_id, &["shared", "prod"]).await,
        IpcResponse::Ok
    ));
    assert!(has_tag(&a, b_id, "prod").await);
    assert_eq!(
        tags_of(&a, b_id).iter().filter(|t| *t == "shared").count(),
        1,
        "a tag both halves carry was listed twice: {:?}",
        tags_of(&a, b_id)
    );

    // Clearing the assignment leaves what the node itself announced.
    assert!(matches!(assign(&a, b_id, &[]).await, IpcResponse::Ok));
    assert!(
        wait_until(Duration::from_secs(15), || !tags_of(&a, b_id)
            .iter()
            .any(|t| t == "prod"))
        .await,
        "clearing did not remove the assigned tag"
    );
    assert!(
        tags_of(&a, b_id).iter().any(|t| t == "shared"),
        "clearing the assignment took the node's own tag with it: {:?}",
        tags_of(&a, b_id)
    );

    // The node editing its own tags does not disturb an assignment.
    assert!(matches!(assign(&a, b_id, &["prod"]).await, IpcResponse::Ok));
    assert!(has_tag(&b, b_id, "prod").await);
    assert!(matches!(
        ipc_as(
            &b,
            founder_user_key(&a),
            IpcRequest::TagsUpdate {
                add: vec!["local".into()],
                remove: vec!["shared".into()],
            },
        )
        .await,
        IpcResponse::Ok
    ));
    assert!(
        has_tag(&b, b_id, "local").await,
        "the node's own edit failed"
    );
    assert!(
        tags_of(&b, b_id).iter().any(|t| t == "prod"),
        "the node's own edit dropped the assignment: {:?}",
        tags_of(&b, b_id)
    );
}

/// A tag only the machine is in a position to know is refused, in words
/// that name the verb that does the real thing. Filtered silently, an
/// operator would believe they had changed something.
#[tokio::test(flavor = "multi_thread")]
async fn tags_the_node_states_about_itself_are_refused() {
    let a = spawn_founder("a", "tags-refuse").await;
    let b = join_new_node("b", &a, vec![]).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let b_id = b.node.node_id();

    for bad in [
        "cap.cores=999",
        mesh_proto::DRAINING_TAG,
        mesh_proto::RELAY_TAG,
        mesh_proto::NO_EXECUTE_TAG,
    ] {
        match assign(&a, b_id, &[bad]).await {
            IpcResponse::Error { message } => assert!(
                message.contains(bad) && message.contains("cannot be assigned from here"),
                "unhelpful refusal for {bad}: {message}"
            ),
            other => panic!("`{bad}` must not be assignable from a distance, got {other:?}"),
        }
    }

    // `no-shell` is the counter-example, and its own documentation is why:
    // it is a policy an operator declares about a machine, and the whole
    // point is not editing TOML on six hundred boxes.
    assert!(matches!(
        assign(&a, b_id, &[mesh_proto::NO_SHELL_TAG]).await,
        IpcResponse::Ok
    ));
    assert!(has_tag(&a, b_id, mesh_proto::NO_SHELL_TAG).await);
}

/// A node nobody has heard of cannot be labelled — the refusal that stops a
/// typo becoming an assignment nothing will ever wear.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_node_is_refused() {
    let a = spawn_founder("a", "tags-unknown").await;
    match assign(&a, NodeId([9u8; 32]), &["prod"]).await {
        IpcResponse::Error { message } => assert!(message.contains("no member"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
