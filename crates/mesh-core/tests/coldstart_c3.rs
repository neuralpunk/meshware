//! C3 — operational cost, proven (C0–C9 §7).
//!
//! The smallest milestone in the cluster direction and the one memo B is
//! loudest about. Its claim is not throughput: it is that there is nothing
//! to stand up. Every row of the operational-cost table is a fact about a
//! competitor's own documentation except the last one, *time from zero to
//! scheduling*, and that one has to be **earned**.
//!
//! So it is measured here, on every change, with a gate — because a number
//! that is only measured when somebody remembers to measure it is a number
//! that quietly regresses.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_testkit::coldstart;

/// The in-process budget. Generous relative to what the harness actually
/// takes, and deliberately so: this gate exists to catch a *regression in
/// kind* — a synchronous fsync added to the join path, a sleep in startup,
/// a handshake that grew a round trip — not to police tens of milliseconds
/// on a shared CI runner. A tight budget on a noisy machine is a gate
/// people learn to re-run until it passes, which is worse than no gate.
const BUDGET: Duration = Duration::from_secs(15);

/// Zero to scheduling, in phases, from a script anyone can run.
///
/// The number this prints is the one `docs/operational-cost.md` publishes
/// for the in-process harness, and the shape it prints is the same shape
/// `scripts/cold-start.sh` fills in on a real host — where the `install`
/// phase, which cannot exist here, is real.
#[tokio::test(flavor = "multi_thread")]
async fn zero_to_scheduling_is_measured_and_bounded() {
    let (report, founder, joiner) = coldstart::measure().await;

    // Printed unconditionally: the point of the harness is the number, and
    // a number only visible on failure is not published.
    println!("cold start: {}", report.summary());
    println!("cold start json: {}", report.to_json());

    assert!(
        report.total() < BUDGET,
        "cold start regressed past {BUDGET:?}: {}",
        report.summary()
    );

    // The measurement has to be of something real, not of an empty
    // function. Two members and a scheduled job is the least that proves
    // the harness measured a working mesh.
    assert_eq!(
        founder.node.engine().read(|s| s.members.len()),
        2,
        "the joiner should be a member by the time the clock stopped"
    );
    assert!(
        founder.node.engine().read(|s| !s.jobs.is_empty()),
        "a job should have been scheduled by the time the clock stopped"
    );
    assert!(
        report.install_micros.is_none(),
        "the in-process harness has nothing to install, and must not \
         report a zero as if it did"
    );
    assert!(report.up_micros > 0 && report.join_micros > 0);
    drop(joiner);
}

/// Every phase is measured, and the total is their sum. A report that
/// silently dropped a phase would still pass the budget — and would be a
/// smaller number for a worse reason, which is exactly the failure mode a
/// published benchmark has to be immune to.
#[test]
fn the_total_is_the_sum_of_the_phases() {
    let report = coldstart::ColdStart {
        install_micros: Some(4_000_000),
        up_micros: 120_000,
        invite_micros: 3_000,
        join_micros: 400_000,
        schedule_micros: 25_000,
    };
    assert_eq!(report.total_micros(), 4_548_000);

    // …and an unmeasured install contributes nothing rather than being
    // guessed at.
    let no_install = coldstart::ColdStart {
        install_micros: None,
        ..report
    };
    assert_eq!(no_install.total_micros(), 548_000);
    assert!(no_install.to_json().contains("\"install_micros\":null"));
    assert!(no_install.summary().starts_with("install - "));
}
