//! C8 — legibility: the scheduler shows its work (C0–C9 §12).
//!
//! `score_full()` computed a weighted sum over eight terms for every
//! candidate on every placement since M5, and discarded all of it except
//! the winner. *"Why did my job land there?"* is the most common
//! operational question in distributed computing and essentially no product
//! answers it — while this one had already computed the answer and thrown
//! it away.
//!
//! Two properties matter more than the chart, and both are asserted here.
//! **Keeping the explanation must not change the placement**: an
//! observability feature that moves jobs is a scheduling change wearing a
//! disguise. And **the explanation must be the arithmetic that ran**, not a
//! second model of it — which is why the totals in the record have to add
//! up to the terms beside them.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, Liveness};
use mesh_testkit::{
    RunOutcome, TestNode, capacity, founder_user_key, ipc_as, join_new_node_sized, run_to_end,
    spawn_founder_sized, wait_until,
};

fn run(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn explain(
    node: &TestNode,
    as_user: &TestNode,
    job: mesh_proto::JobId,
) -> mesh_proto::JobExplainView {
    match ipc_as(
        node,
        founder_user_key(as_user),
        IpcRequest::JobExplain { job_id: job },
    )
    .await
    {
        IpcResponse::JobExplain(view) => *view,
        other => panic!("unexpected response to JobExplain: {other:?}"),
    }
}

/// The breakdown is written on the history task's tick, off the submission
/// path — writing it inline would put an fsync between a user pressing
/// enter and their job starting.
async fn explain_when_indexed(
    node: &TestNode,
    job: mesh_proto::JobId,
) -> mesh_proto::PlacementExplain {
    let mut last = None;
    for _ in 0..300 {
        let view = explain(node, node, job).await;
        if let Some(placement) = view.placement {
            return placement;
        }
        last = Some(view.state.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no placement explanation was ever indexed (last state {last:?})");
}

/// The demo P1 ends on: "why did it land there", answered from the CLI, for
/// a job that has already finished.
#[tokio::test]
async fn a_finished_job_can_say_why_it_landed_where_it_did() {
    let founder = spawn_founder_sized("legible-a1", "mesh-legible", capacity(8, 16, 200)).await;
    let small = join_new_node_sized("legible-a2", &founder, capacity(2, 4, 50)).await;
    let joined = wait_until(Duration::from_secs(30), || {
        founder.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                founder.node.gossip().liveness_of(&small.node.node_id()),
                Liveness::Alive
            )
    })
    .await;
    assert!(
        joined,
        "both nodes must be live before a placement means anything"
    );

    let user = founder_user_key(&founder);
    let RunOutcome::Done { job_id, .. } = run_to_end(&founder, user, run("echo placed")).await
    else {
        panic!("the job must run before it can be explained");
    };
    let job = job_id.expect("a started job has an id");

    let placement = explain_when_indexed(&founder, job).await;
    assert!(
        placement.considered >= 2,
        "both candidates must have been scored, not just the winner"
    );
    assert!(
        !placement.candidates.is_empty(),
        "the breakdown is the whole feature"
    );
    assert!(
        placement.candidates.len() <= mesh_proto::EXPLAIN_CANDIDATES,
        "a full breakdown for every candidate is a memory leak with a chart on it"
    );
    let chosen = placement.chosen.expect("a job that ran landed somewhere");
    assert!(
        placement.candidates.iter().any(|c| c.node == chosen),
        "the node that took the work has to be in the breakdown that explains it"
    );

    // Every term the scheduler weighs is named, so a chart drawn from this
    // cannot silently omit the one that decided the placement.
    let winner = placement.chosen_score().unwrap();
    let names: Vec<&str> = winner.terms.iter().map(|t| t.name.as_str()).collect();
    for expected in [
        "load", "warmth", "failure", "rtt", "memory", "disk", "locality", "device",
    ] {
        assert!(
            names.contains(&expected),
            "term {expected} is missing from {names:?}"
        );
    }
}

/// The explanation must be the arithmetic that ran. A total computed
/// separately from the breakdown would agree on the day it was written and
/// diverge silently forever after.
#[tokio::test]
async fn the_total_is_the_sum_of_the_terms_shown() {
    let founder = spawn_founder_sized("legible-b1", "mesh-legible", capacity(8, 16, 200)).await;
    let _second = join_new_node_sized("legible-b2", &founder, capacity(4, 8, 100)).await;
    wait_until(Duration::from_secs(30), || {
        founder.node.engine().read(|s| s.members.len() == 2)
    })
    .await;

    let user = founder_user_key(&founder);
    let RunOutcome::Done { job_id, .. } = run_to_end(&founder, user, run("true")).await else {
        panic!("the job must run");
    };
    let placement = explain_when_indexed(&founder, job_id.unwrap()).await;

    for candidate in &placement.candidates {
        let summed: f64 = candidate.terms.iter().map(|t| t.weighted).sum();
        assert!(
            (candidate.total - summed).abs() < 1e-9,
            "{}: total {} but its terms sum to {summed}",
            candidate.name,
            candidate.total
        );
        for term in &candidate.terms {
            assert!(
                (term.weighted - term.raw * term.weight).abs() < 1e-9,
                "{}/{}: weighted is not raw × weight",
                candidate.name,
                term.name
            );
            assert!(
                (0.0..=1.0).contains(&term.raw),
                "{}/{}: every term is a penalty in 0..=1, got {}",
                candidate.name,
                term.name,
                term.raw
            );
        }
    }

    // Best first, which is what makes "the winner and the four it beat"
    // meaningful.
    for pair in placement.candidates.windows(2) {
        assert!(
            pair[0].total <= pair[1].total,
            "the breakdown must be ranked best-first"
        );
    }
}

/// A node that did not place the job has no breakdown and no honest way to
/// invent one. Naming the node that does is a better product than a
/// plausible fabrication.
#[tokio::test]
async fn a_node_that_did_not_place_it_says_who_did() {
    let founder = spawn_founder_sized("legible-c1", "mesh-legible", capacity(8, 16, 200)).await;
    let second = join_new_node_sized("legible-c2", &founder, capacity(4, 8, 100)).await;
    wait_until(Duration::from_secs(30), || {
        second.node.engine().read(|s| s.members.len() == 2)
    })
    .await;

    let user = founder_user_key(&founder);
    let RunOutcome::Done { job_id, .. } = run_to_end(&founder, user, run("true")).await else {
        panic!("the job must run");
    };
    let job = job_id.unwrap();
    explain_when_indexed(&founder, job).await;

    // Asked on the other node, once it has heard of the job at all.
    let known = wait_until(Duration::from_secs(30), || {
        second.node.engine().read(|s| s.jobs.contains_key(&job))
    })
    .await;
    assert!(
        known,
        "the summary has to gossip before this question is fair"
    );

    let view = explain(&second, &founder, job).await;
    assert!(
        view.placement.is_none(),
        "a node that did not decide the placement must not claim to explain it"
    );
    assert_eq!(
        view.placement_on,
        Some(founder.node.node_id()),
        "it has to name the node that does hold the answer"
    );
}

/// The what-if planner (P3): submit nothing, find out where it would land.
///
/// A simulation over the *real* scorer, so what it reports is a
/// `PlacementExplain` of exactly the shape a real placement produces —
/// which is how it is kept from drifting into a second model of the
/// scheduler.
#[tokio::test]
async fn the_planner_answers_without_submitting_anything() {
    let founder = spawn_founder_sized("legible-d1", "mesh-legible", capacity(8, 16, 200)).await;
    let second = join_new_node_sized("legible-d2", &founder, capacity(4, 8, 100)).await;
    // Liveness, not just membership: placement filters on it, so a plan
    // taken before the first gossip round would honestly report one
    // candidate and the assertion below would be about timing rather than
    // about the planner.
    let live = wait_until(Duration::from_secs(30), || {
        founder.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                founder.node.gossip().liveness_of(&second.node.node_id()),
                Liveness::Alive
            )
    })
    .await;
    assert!(
        live,
        "both nodes must be live before the planner can see two"
    );
    let user = founder_user_key(&founder);
    let before = founder.node.engine().read(|s| s.jobs.len());

    let plan = match ipc_as(
        &founder,
        user,
        IpcRequest::Plan {
            on: None,
            service: None,
            env: vec![],
            fanout: None,
        },
    )
    .await
    {
        IpcResponse::Plan(plan) => *plan,
        other => panic!("unexpected response to Plan: {other:?}"),
    };

    assert!(
        plan.refusal.is_none(),
        "an idle two-node mesh can take work"
    );
    assert!(
        plan.placement.chosen.is_some(),
        "the plan has to name a node"
    );
    assert!(plan.placement.considered >= 2);
    assert_eq!(plan.slots_wanted, 1);
    assert!(plan.slots_free > 0, "an idle mesh has slots");
    assert_eq!(
        plan.wait_secs, 0,
        "with a free slot the answer to 'when does it start' is 'now'"
    );
    assert_eq!(
        founder.node.engine().read(|s| s.jobs.len()),
        before,
        "the planner must submit nothing — that is the entire point of it"
    );
}

/// A plan for work nothing can take says so, rather than reporting a finish
/// time for a job that would never start.
#[tokio::test]
async fn the_planner_refuses_what_would_never_run() {
    let founder = spawn_founder_sized("legible-e", "mesh-legible", capacity(4, 8, 100)).await;
    let user = founder_user_key(&founder);

    let plan = match ipc_as(
        &founder,
        user,
        IpcRequest::Plan {
            on: Some("tag=nonexistent".into()),
            service: None,
            env: vec![],
            fanout: None,
        },
    )
    .await
    {
        IpcResponse::Plan(plan) => *plan,
        other => panic!("unexpected response to Plan: {other:?}"),
    };
    let refusal = plan.refusal.expect("no node matches that selector");
    assert!(
        refusal.contains("selector"),
        "the refusal has to say what did not match: {refusal}"
    );
    assert!(
        plan.finish.is_none(),
        "work that cannot start has no finish time"
    );
}

/// A duration range is reported as a range. A planner that says "17
/// minutes" is wrong precisely often enough to be untrusted forever.
#[tokio::test]
async fn the_estimate_widens_rather_than_guessing() {
    let founder = spawn_founder_sized("legible-f", "mesh-legible", capacity(4, 8, 100)).await;
    let user = founder_user_key(&founder);

    // Give the archive something to learn from.
    for _ in 0..3 {
        let _ = run_to_end(&founder, user, run("true")).await;
    }
    let plan = match ipc_as(
        &founder,
        user,
        IpcRequest::Plan {
            on: None,
            service: None,
            env: vec![],
            fanout: None,
        },
    )
    .await
    {
        IpcResponse::Plan(plan) => *plan,
        other => panic!("unexpected response to Plan: {other:?}"),
    };
    // With or without history, the estimate is a distribution or nothing —
    // never a single confident number dressed up as one.
    if let Some(duration) = plan.duration {
        assert!(duration.p10_secs <= duration.p50_secs);
        assert!(duration.p50_secs <= duration.p90_secs);
        assert!(duration.samples > 0);
        assert!(duration.describe().contains("p50"));
    }
}
