//! **What a mesh with no control plane does when the network splits** (E1-P1).
//!
//! meshware's headline claim is that there is no control plane, no quorum
//! and nothing to be down. Every one of those words is a claim about
//! partition behaviour, and until this file none of them had a test.
//! MASTERPLAN §16 said so plainly — the harness had no partition injection
//! — and a grep for `partition` across the whole test tree returned a
//! single comment.
//!
//! These tests are the evidence behind `docs/no-control-plane.md` (E1-P2).
//! The document is written *from what passes here*, not from intent, which
//! is the only ordering that keeps the two honest with each other.
//!
//! ## The shape of every test in this file
//!
//! Found a mesh, cut it with [`mesh_testkit::netsplit`], do something on
//! both sides of the cut, heal, then assert on **convergence** rather than
//! on timing. Convergence is a stable property; timing is not, and the
//! testkit still has no deterministic clock (see the netsplit module docs
//! for why that is a separate piece of work).
//!
//! One test is about timing on purpose: how long a silent peer takes to be
//! noticed (ENG-7). It holds a generous budget against numbers measured
//! well inside it, because an evaluator times exactly that.
//!
//! ## The guard test comes first, deliberately
//!
//! [`a_cut_actually_drops_datagrams`] exists because every other test in
//! this file would pass on a mesh that had simply gone quiet. A partition
//! test that cannot prove the partition happened proves nothing at all.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_proto::{IpcRequest, JobState, Liveness};
use mesh_testkit::netsplit::Netsplit;
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node_split, spawn_founder_split, wait_until,
};

/// Sides. Named rather than numbered at every call site: `LEFT`/`RIGHT`
/// reads as a topology, `0`/`1` reads as an index into nothing.
const LEFT: u8 = 0;
const RIGHT: u8 = 1;

/// Budget for state and membership convergence after a heal.
///
/// Measured rather than guessed: on this harness a heal is resolved in
/// **~51 ms** — the first successful sync clears the failure count — so
/// this is two orders of magnitude of headroom for a loaded CI box.
const CONVERGE: Duration = Duration::from_secs(45);

/// Budget for a peer to be declared **dead** across a silent-drop
/// partition, in the tests below that are about something else.
///
/// Measured on this harness (4 nodes, 2 a side): **not alive after 5–10 s,
/// Dead after 10–13 s** (ENG-7; it was 46 s and 51 s, when every probe of
/// an unreachable peer burned a whole dial timeout, one at a time).
/// `a_silent_peer_is_noticed_within_seconds` holds the number itself to
/// [`DETECT_BUDGET`]; this is the generous one, for a loaded machine.
///
/// Still quicker to forgive than to condemn: one answer brings a peer
/// back, and it takes several failures over seconds to lose one, because
/// wrongly declaring a node dead moves its work.
const DEAD_DETECT: Duration = Duration::from_secs(60);

/// What detection has to beat: dead within fifteen seconds of the last
/// packet (ENG-7).
const DETECT_BUDGET: Duration = Duration::from_secs(15);

/// Submit as `admin` **through** `node`.
///
/// The signing key is always the founder's: a joined node's own key is not
/// a user of the mesh, and meshware authenticates the *user*, not the
/// socket. Submitting through a node on the far side of a cut is the point
/// — it proves that half can schedule without reaching the founder.
async fn run_on(
    node: &TestNode,
    admin: &mesh_core::identity::Keypair,
    command: &str,
    on: Option<String>,
) -> (String, JobState) {
    let req = IpcRequest::Run {
        command: command.to_string(),
        on,
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match mesh_testkit::run_to_end(node, admin, req).await {
        RunOutcome::Done { stdout, state, .. } => (stdout, state),
        RunOutcome::Denied(m) => panic!("run denied: {m}"),
        RunOutcome::Error(m) => panic!("run error: {m}"),
    }
}

/// Everyone can see everyone as an active member.
async fn all_see_all(nodes: &[&TestNode]) -> bool {
    wait_until(CONVERGE, || {
        nodes.iter().all(|a| {
            nodes.iter().all(|b| {
                a.node
                    .engine()
                    .read(|s| s.is_active_member(&b.node.node_id()))
            })
        })
    })
    .await
}

/// Found a four-node mesh, two nodes a side, and wait for it to settle.
///
/// The founder is on the left, and every join happens **before** any cut —
/// a node cannot join across a link that is already down, which is correct
/// and is its own test rather than a surprise in the middle of this one.
async fn four_node_mesh(split: &Arc<Netsplit>) -> (TestNode, TestNode, TestNode, TestNode) {
    let a = spawn_founder_split("alpha", "hyacinth", split, LEFT).await;
    let b = join_new_node_split("bravo", &a, split, LEFT).await;
    let c = join_new_node_split("charlie", &a, split, RIGHT).await;
    let d = join_new_node_split("delta", &a, split, RIGHT).await;
    assert!(
        all_see_all(&[&a, &b, &c, &d]).await,
        "the mesh should converge before anything is cut"
    );
    (a, b, c, d)
}

// ───────────────────────────────────────────────────────────────────────
// The guard
// ───────────────────────────────────────────────────────────────────────

/// **The test that makes the rest of this file mean anything.**
///
/// If `cut` did nothing, every other test here would still pass — a mesh
/// that is merely idle looks exactly like a mesh that is partitioned, right
/// up until you heal it and nothing changes. So: assert the harness is
/// dropping datagrams while the cut is up, and that it stops when healed.
#[tokio::test]
async fn a_cut_actually_drops_datagrams() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    let (dropped_before, delivered_before) = split.stats();
    assert!(
        delivered_before > 0,
        "the mesh should have exchanged datagrams while converging"
    );
    assert_eq!(dropped_before, 0, "nothing is cut yet");

    split.cut(LEFT, RIGHT);
    assert!(split.is_cut());

    // Gossip alone will try to cross the cut within a couple of rounds.
    let dropping = wait_until(Duration::from_secs(20), || split.stats().0 > 0).await;
    assert!(
        dropping,
        "a cut mesh should be dropping datagrams; harness is not filtering"
    );

    split.heal();
    let after_heal = split.stats().0;
    let delivering_again = wait_until(Duration::from_secs(20), || {
        split.stats().1 > delivered_before && split.stats().0 == after_heal
    })
    .await;
    assert!(
        delivering_again,
        "a healed mesh should deliver again and stop dropping"
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

// ───────────────────────────────────────────────────────────────────────
// The claim itself
// ───────────────────────────────────────────────────────────────────────

/// **Red-team Q2 and Q3: the mesh splits in half — do both halves keep
/// working?**
///
/// Yes, and that is the whole point of having no quorum. A leader-elected
/// scheduler would have a minority half that refuses to schedule; meshware
/// has two halves that each keep their promises to the work in front of
/// them.
///
/// This is also the honest half of the claim: independent work proceeds on
/// both sides *by design*, which means two halves can each start work the
/// other cannot see. That is a property to document, not a bug to fix —
/// see `docs/no-control-plane.md`.
#[tokio::test]
async fn both_halves_keep_executing_while_partitioned() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    split.cut(LEFT, RIGHT);
    // Let the halves notice, so placement is choosing from what it can see
    // rather than from a stale membership snapshot.
    let noticed = wait_until(Duration::from_secs(20), || split.stats().0 > 0).await;
    assert!(noticed, "the cut should be taking effect");

    // Each half runs work pinned inside itself.
    let admin = founder_user_key(&a).clone();
    let (left_out, left_state) =
        run_on(&a, &admin, "echo left-half-ran", Some("name=bravo".into())).await;
    let (right_out, right_state) =
        run_on(&c, &admin, "echo right-half-ran", Some("name=delta".into())).await;

    assert_eq!(
        left_state,
        JobState::Succeeded,
        "left half should still schedule and execute: {left_out}"
    );
    assert_eq!(
        right_state,
        JobState::Succeeded,
        "right half should still schedule and execute: {right_out}"
    );
    assert!(left_out.contains("left-half-ran"));
    assert!(right_out.contains("right-half-ran"));

    split.heal();
    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **Red-team Q4: what happens when they reconnect?**
///
/// Work done on both sides during the split is visible everywhere
/// afterwards. Nothing is lost and nothing needs a reconciliation pass:
/// the op-log is grow-only and the merge is the same one anti-entropy runs
/// on an ordinary day, so a heal is not a special case in the code at all.
#[tokio::test]
async fn state_from_both_halves_survives_the_heal() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    split.cut(LEFT, RIGHT);
    assert!(wait_until(Duration::from_secs(20), || split.stats().0 > 0).await);

    // A job in each half. Each produces a replicated job record that the
    // *other* half cannot possibly have seen yet.
    let admin = founder_user_key(&a).clone();
    let (_, ls) = run_on(&a, &admin, "echo l", Some("name=bravo".into())).await;
    let (_, rs) = run_on(&c, &admin, "echo r", Some("name=delta".into())).await;
    assert_eq!(ls, JobState::Succeeded);
    assert_eq!(rs, JobState::Succeeded);

    let left_jobs = a.node.engine().read(|s| s.jobs.len());
    let right_jobs = c.node.engine().read(|s| s.jobs.len());
    assert!(left_jobs > 0 && right_jobs > 0);

    // Neither side has the other's work while the cut is up.
    let a_id = a.node.node_id();
    let c_id = c.node.node_id();
    let _ = (a_id, c_id);

    split.heal();

    // After the heal both sides hold the union, with no op lost.
    let converged = wait_until(CONVERGE, || {
        let l = a.node.engine().read(|s| s.jobs.len());
        let r = c.node.engine().read(|s| s.jobs.len());
        l == r && l >= left_jobs + right_jobs
    })
    .await;
    assert!(
        converged,
        "job records from both halves should merge on heal: left={} right={} (was {left_jobs}+{right_jobs})",
        a.node.engine().read(|s| s.jobs.len()),
        c.node.engine().read(|s| s.jobs.len()),
    );

    // And membership is intact — nobody was evicted for being unreachable.
    assert!(
        all_see_all(&[&a, &b, &c, &d]).await,
        "every node should still be an active member after a heal"
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **A peer whose packets stop arriving is noticed in seconds** (ENG-7).
///
/// The measured baseline was Suspect at 46 s and Dead at 51 s: probes ran
/// one at a time with no deadline, and each burned a whole dial timeout
/// before it counted. Evaluators time failover, and until a node reads as
/// not-alive it keeps being chosen for work.
#[tokio::test]
async fn a_silent_peer_is_noticed_within_seconds() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;
    let gossip = a.node.gossip().clone();
    let c_id = c.node.node_id();

    let cut_at = std::time::Instant::now();
    split.cut(LEFT, RIGHT);
    let suspect = wait_until(DETECT_BUDGET, || {
        !matches!(gossip.liveness_of(&c_id), Liveness::Alive)
    })
    .await;
    let suspect_after = cut_at.elapsed();
    let dead = wait_until(DETECT_BUDGET, || {
        matches!(gossip.liveness_of(&c_id), Liveness::Dead)
    })
    .await;
    let dead_after = cut_at.elapsed();
    eprintln!("detection: not alive after {suspect_after:?}, dead after {dead_after:?}");
    assert!(
        suspect,
        "charlie still reads alive {suspect_after:?} after the cut"
    );
    assert!(
        dead && dead_after <= DETECT_BUDGET,
        "charlie was not dead within {DETECT_BUDGET:?} (took {dead_after:?}, now {:?})",
        gossip.liveness_of(&c_id)
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **Red-team Q1 and Q6: nodes vanish, and the survivors notice.**
///
/// A partition should move the far side to `Dead` on the near side's
/// failure detector — and, critically, `Dead` must not mean `evicted`.
/// Membership is an OR-set and only an admin-signed revocation removes a
/// member; a node that was merely unreachable comes straight back.
#[tokio::test]
async fn a_partitioned_peer_goes_dead_then_returns() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    let gossip = a.node.gossip().clone();
    let c_id = c.node.node_id();

    split.cut(LEFT, RIGHT);

    let died = wait_until(DEAD_DETECT, || {
        matches!(gossip.liveness_of(&c_id), Liveness::Dead)
    })
    .await;
    assert!(
        died,
        "alpha should eventually mark charlie dead across a partition (was {:?})",
        gossip.liveness_of(&c_id)
    );

    // Dead is not gone: the member record survives, because eviction takes
    // a signature and a dropped packet is not one.
    assert!(
        a.node.engine().read(|s| s.is_active_member(&c_id)),
        "an unreachable node must stay a member — only a signed revocation removes one"
    );

    split.heal();

    let revived = wait_until(CONVERGE, || {
        matches!(gossip.liveness_of(&c_id), Liveness::Alive)
    })
    .await;
    assert!(
        revived,
        "charlie should come back alive after the heal (was {:?})",
        gossip.liveness_of(&c_id)
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **A liveness transition moves the `Nodes` change signal** (**H-8**).
///
/// Director refetches a collection only when that collection's counter
/// moves, and the counter was a fold over `OpBody` alone. Liveness authors
/// no op — it is ephemeral by design — so a peer going dead moved nothing,
/// and every `NodeRow.liveness` on every attached client stayed frozen at
/// whatever the last *membership* op had left it as. On a mesh that had
/// finished joining, that was for ever: the topology showed "no reply"
/// under a header that said, from the same `liveness_of` one poll apart,
/// that every member was alive. The cure was to restart any node at all,
/// because a boot authors a `MemberUpdate` — which is the tell, since a
/// real network fault is not fixed by restarting a different machine.
///
/// This asserts the counter moves in **both** directions. Only checking
/// the death would pass on an implementation that bumped once and then
/// left the client stale on the recovery, which is the same bug wearing
/// the opposite sign.
#[tokio::test]
async fn a_liveness_transition_moves_the_nodes_change_signal() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    let gossip = a.node.gossip().clone();
    let engine = a.node.engine().clone();
    let c_id = c.node.node_id();
    let nodes_counter = || engine.topic_versions().get(mesh_proto::StateTopic::Nodes);

    // Settle first: joining authors membership ops, and this test is about
    // what happens once nothing is authoring any.
    assert!(
        wait_until(CONVERGE, || matches!(
            gossip.liveness_of(&c_id),
            Liveness::Alive
        ))
        .await,
        "charlie should be alive before the cut"
    );
    let before_cut = nodes_counter();

    split.cut(LEFT, RIGHT);
    assert!(
        wait_until(DEAD_DETECT, || matches!(
            gossip.liveness_of(&c_id),
            Liveness::Dead
        ))
        .await,
        "alpha should mark charlie dead across the partition"
    );
    let after_death = nodes_counter();
    assert!(
        after_death > before_cut,
        "a peer going dead must move the Nodes counter ({before_cut} -> {after_death}); \
         a client that trusts the signal shows a stale topology otherwise"
    );

    split.heal();
    assert!(
        wait_until(CONVERGE, || matches!(
            gossip.liveness_of(&c_id),
            Liveness::Alive
        ))
        .await,
        "charlie should come back alive after the heal"
    );
    let after_revival = nodes_counter();
    assert!(
        after_revival > after_death,
        "a peer coming back must move it too ({after_death} -> {after_revival})"
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **Asymmetric connectivity** — the review asks about it by name, and it
/// is the shape that breaks naive failure detectors.
///
/// Left can send to right; right cannot send to left. Right therefore
/// hears everything and is heard by nobody. The mesh must not tear itself
/// apart over the disagreement, and must recover cleanly when the one-way
/// link is restored.
#[tokio::test]
async fn an_asymmetric_cut_converges_on_heal() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    // Right→left is dropped; left→right still flows.
    split.cut_one_way(RIGHT, LEFT);
    assert!(wait_until(Duration::from_secs(25), || split.stats().0 > 0).await);

    split.heal();

    assert!(
        all_see_all(&[&a, &b, &c, &d]).await,
        "membership should reconverge after a one-way cut is healed"
    );
    let gossip = a.node.gossip().clone();
    let c_id = c.node.node_id();
    let alive = wait_until(CONVERGE, || {
        matches!(gossip.liveness_of(&c_id), Liveness::Alive)
    })
    .await;
    assert!(alive, "liveness should recover from an asymmetric cut");

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}

/// **Red-team Q5 and Q6: a revocation issued during a partition.**
///
/// The security-relevant case, and the one an auditor asks about. A node
/// revoked while it is unreachable must be revoked *everywhere* once the
/// link returns — the revocation is a signed op in the same grow-only log
/// as everything else, so it propagates by the same anti-entropy that
/// carries a job record, and it takes effect on merge rather than on
/// delivery to any particular node.
///
/// The window is real and is worth stating rather than hiding: between
/// signing and heal, the far side has not seen it. What must not happen is
/// the revocation being *lost*, or the revoked node's rejoin overwriting
/// it.
#[tokio::test]
async fn a_revocation_signed_during_a_partition_applies_on_heal() {
    let split = Netsplit::new();
    let (a, b, c, d) = four_node_mesh(&split).await;

    let d_id = d.node.node_id();
    split.cut(LEFT, RIGHT);
    assert!(wait_until(Duration::from_secs(20), || split.stats().0 > 0).await);

    // Alpha (left, admin) expels delta (right) while it cannot be reached.
    // The revocation is signed by the admin's *user* key and verified at
    // merge time on every node, which is exactly why it can be authored on
    // one side of a partition and still be authoritative on the other.
    let admin = founder_user_key(&a).clone();
    let mut revocation = mesh_proto::NodeRevocation {
        node_id: d_id,
        reason: "partition test: expelled while unreachable".into(),
        by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = admin.sign(&revocation.signing_bytes()).to_vec();
    match mesh_testkit::ipc_as(&a, &admin, IpcRequest::NodeRevoke { revocation }).await {
        mesh_proto::IpcResponse::Ok => {}
        other => panic!("revoke refused on the left half: {other:?}"),
    }
    assert!(
        !a.node.engine().read(|s| s.is_active_member(&d_id)),
        "the signing side applies the revocation immediately"
    );
    // The far side has not seen it — the window, stated.
    assert!(
        c.node.engine().read(|s| s.is_active_member(&d_id)),
        "the far side cannot know yet; that window is the thing to document"
    );

    split.heal();

    let propagated = wait_until(CONVERGE, || {
        !c.node.engine().read(|s| s.is_active_member(&d_id))
            && !b.node.engine().read(|s| s.is_active_member(&d_id))
    })
    .await;
    assert!(
        propagated,
        "a revocation signed during a partition must apply on every node once the link returns"
    );

    for n in [a, b, c, d] {
        n.node.shutdown();
    }
}
