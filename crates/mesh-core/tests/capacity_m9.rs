//! M9 P1: nodes announce what hardware they actually have (§5.1).
//!
//! Capacity rides reserved `cap.*` tags on the existing `MemberUpdate`, so
//! there is no new op and no wire change — the properties worth pinning are
//! that the values are true, that they converge like any other membership
//! fact, that an operator cannot forge or drop them, and that they are inert
//! to every existing tag consumer.

use std::time::Duration;

use mesh_proto::{Capacity, IpcRequest, IpcResponse, Liveness};
use mesh_testkit::{
    capacity, founder_user_key, ipc_as, join_new_node_sized, spawn_founder, spawn_founder_sized,
    wait_until,
};

/// Both members visible and alive from `node`.
async fn converged(node: &mesh_testkit::TestNode, others: &[&mesh_testkit::TestNode]) -> bool {
    wait_until(Duration::from_secs(15), || {
        node.node
            .engine()
            .read(|s| s.members.len() == others.len() + 1)
            && others.iter().all(|o| {
                matches!(
                    node.node.gossip().liveness_of(&o.node.node_id()),
                    Liveness::Alive
                )
            })
    })
    .await
}

/// `meshnode nodes` as seen from `node`. The founder's key is the mesh's
/// only registered user, so it signs from whichever node we are asking.
async fn node_rows(
    node: &mesh_testkit::TestNode,
    user: &mesh_core::identity::Keypair,
) -> Vec<mesh_proto::NodeRow> {
    match ipc_as(node, user, IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("expected Nodes, got {other:?}"),
    }
}

/// A node with no declared capacity reports the machine it is actually on.
#[tokio::test(flavor = "multi_thread")]
async fn boot_announces_real_hardware() {
    let a = spawn_founder("a", "m9-probe").await;
    let rows = node_rows(&a, founder_user_key(&a)).await;
    let me = rows.iter().find(|r| r.is_self).expect("self row");
    let cap = me.capacity.as_ref().expect("boot announced capacity");

    let expected_cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .expect("host reports its parallelism");
    assert_eq!(
        cap.cores, expected_cores,
        "cores are the machine's, not a guess"
    );
    assert_eq!(cap.arch, std::env::consts::ARCH);
    if cfg!(target_os = "linux") {
        assert!(cap.mem_mb > 0, "MemTotal is readable on linux");
        assert!(cap.disk_mb > 0, "the data dir lives on a real filesystem");
        // ENG-23: what this node announces is bounded by its cgroup, not
        // by the machine. `/proc/meminfo` is not namespaced, so a node in
        // a container that trusted it advertised the host's memory, took
        // work sized for it and was OOM-killed — and ENG-1 restarted it to
        // do the same again.
        //
        // This runs under whatever limit the harness was given: inside a
        // `systemd-run -p MemoryMax=…` scope there is one and the
        // assertion bites; on a bare run there is none and the machine is
        // the bound. Both are the same rule.
        if let Some((limit_mb, _)) = mesh_core::cgroup::memory_limit() {
            assert!(
                cap.mem_mb <= limit_mb,
                "announced {} MB under a {limit_mb} MB cgroup limit",
                cap.mem_mb
            );
        }
        let machine = mesh_core::capacity::machine_mem_total_mb().expect("MemTotal");
        assert!(
            cap.mem_mb <= machine,
            "announced {} MB on a {machine} MB machine",
            cap.mem_mb
        );
    }

    // The raw tags never reach a user-facing tag list — capacity has a
    // column of its own.
    assert!(
        !me.tags.iter().any(|t| t.starts_with("cap.")),
        "cap.* leaked into user tags: {:?}",
        me.tags
    );
}

/// The headline P1 property: two nodes of different size each report their
/// own hardware, and every node sees both.
#[tokio::test(flavor = "multi_thread")]
async fn heterogeneous_capacity_converges_mesh_wide() {
    let big = capacity(32, 128, 900);
    let small = capacity(16, 64, 500);
    let a = spawn_founder_sized("merope", "m9-hetero", big.clone()).await;
    let b = join_new_node_sized("nim", &a, small.clone()).await;
    let user = founder_user_key(&a).clone();

    assert!(converged(&a, &[&b]).await, "membership did not converge");
    assert!(converged(&b, &[&a]).await, "membership did not converge");

    // Both nodes agree, from either side of the mesh.
    for viewer in [&a, &b] {
        assert!(
            wait_until(Duration::from_secs(15), || {
                viewer.node.engine().read(|s| {
                    s.members
                        .values()
                        .filter_map(|m| Capacity::from_tags(&m.info.tags))
                        .count()
                        == 2
                })
            })
            .await,
            "capacity did not propagate to {}",
            viewer.node.node_id().short()
        );
        let rows = node_rows(viewer, &user).await;
        let by_name = |name: &str| -> Capacity {
            rows.iter()
                .find(|r| r.name == name)
                .and_then(|r| r.capacity.clone())
                .unwrap_or_else(|| panic!("no capacity for {name}"))
        };
        assert_eq!(by_name("merope"), big);
        assert_eq!(by_name("nim"), small);
    }
}

/// Capacity is runtime-managed: an operator can neither forge it nor drop
/// it, and an ordinary tag edit leaves it untouched.
#[tokio::test(flavor = "multi_thread")]
async fn tag_edits_cannot_clobber_capacity() {
    let declared = capacity(8, 32, 250);
    let a = spawn_founder_sized("a", "m9-tags", declared.clone()).await;
    let user = founder_user_key(&a).clone();
    let me = a.node.node_id();

    for bad in [
        IpcRequest::TagsUpdate {
            add: vec!["cap.cores=999".into()],
            remove: vec![],
        },
        IpcRequest::TagsUpdate {
            add: vec![],
            remove: vec!["cap.cores=8".into()],
        },
    ] {
        match ipc_as(&a, &user, bad).await {
            IpcResponse::Error { message } => {
                assert!(message.contains("cap."), "unhelpful refusal: {message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // An ordinary tag edit still works, and capacity survives it.
    let ok = ipc_as(
        &a,
        &user,
        IpcRequest::TagsUpdate {
            add: vec!["gpu-box".into()],
            remove: vec![],
        },
    )
    .await;
    assert!(matches!(ok, IpcResponse::Ok), "tag add failed: {ok:?}");

    let tags = a
        .node
        .engine()
        .read(|s| s.members.get(&me).map(|m| m.info.tags.clone()))
        .expect("self member");
    assert_eq!(Capacity::from_tags(&tags), Some(declared));
    assert!(tags.iter().any(|t| t == "gpu-box"));
}

/// `cap.*` is inert to everything that already reads tags: a node carrying
/// capacity is still an ordinary placement candidate, and is neither
/// no-execute nor draining. This is the same property that makes the tags
/// safe for a 1.5 peer, which matches reserved tags by exact string.
#[tokio::test(flavor = "multi_thread")]
async fn capacity_tags_are_inert_to_existing_tag_consumers() {
    let a = spawn_founder_sized("a", "m9-inert", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("b", &a, capacity(2, 4, 50)).await;
    assert!(converged(&a, &[&b]).await, "membership did not converge");

    let targets = a.node.engine().read(|s| {
        mesh_core::membership::resolve_targets(
            s,
            &mesh_core::membership::Selector::Any,
            a.node.node_id(),
            |id| a.node.gossip().liveness_of(id),
        )
    });
    assert_eq!(
        targets.len(),
        2,
        "capacity-tagged nodes must stay placement candidates"
    );

    // Selecting by an ordinary tag is unaffected by the extra tags.
    let none = a.node.engine().read(|s| {
        mesh_core::membership::resolve_targets(
            s,
            &mesh_core::membership::Selector::Tag("nonexistent".into()),
            a.node.node_id(),
            |id| a.node.gossip().liveness_of(id),
        )
    });
    assert!(none.is_empty());
}
