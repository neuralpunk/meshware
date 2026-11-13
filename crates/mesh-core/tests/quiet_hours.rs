//! Quiet hours: time-boxed slot caps, in local time (`mesh_core::quiet`).
//!
//! The unit tests in `quiet.rs` and `localtime.rs` pin the arithmetic —
//! windows, wrapping, daylight saving, the parser's tolerance for how
//! people actually type. What can only be pinned against a running engine
//! is the part that matters to an operator:
//!
//! - a capped node **refuses** rather than queueing forever, so the origin
//!   spills to a neighbour through the path `Busy` already takes;
//! - a 0% node is still a full member — it gossips, holds state, and is a
//!   perfectly good origin for work that runs somewhere else;
//! - work already running is **never** killed by a window opening;
//! - the cap is live: SIGHUP-equivalent reload moves it without a restart.
//!
//! Windows here are pinned to the current local minute rather than to a
//! clock the test controls, because the node reads the real one. `covering`
//! and `avoiding` build a window that certainly does or certainly does not
//! contain "now", including across midnight, which is the case a naïve
//! `now±1h` gets wrong for an hour a day.

use std::time::Duration;

use mesh_core::quiet::QuietHours;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority};
use mesh_testkit::{founder_user_key, ipc_as, spawn_founder_with, stream_as, wait_until};

/// The local minute-of-day, as the node under test computes it.
fn now_minute() -> u32 {
    mesh_core::localtime::LocalZone::local().minute_of_day(mesh_core::localtime::now_unix())
}

fn hhmm(minute: i64) -> String {
    let m = minute.rem_euclid(1440);
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// A window certainly covering right now, capped at `pct`.
///
/// Two hours wide and centred on the current minute, so the test cannot
/// race the clock across its own edge even on a slow machine.
fn covering(pct: u8) -> String {
    let now = i64::from(now_minute());
    format!("{}-{} @ {pct}%", hhmm(now - 60), hhmm(now + 60))
}

/// A window certainly *not* covering right now.
fn avoiding(pct: u8) -> String {
    let now = i64::from(now_minute());
    format!("{}-{} @ {pct}%", hhmm(now + 300), hhmm(now + 420))
}

fn run_req(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: Some("any".into()),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// A node whose only executor is quiet takes no new work.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_percent_window_refuses_new_work() {
    let a = spawn_founder_with("a", "quiet-refuse", vec![], |c| {
        c.limits.slots = 4;
        c.quiet = QuietHours::parse(&covering(0)).expect("window parses");
    })
    .await;

    assert_eq!(
        a.node.jobs().effective_slots(),
        0,
        "a 0% window leaves no slots"
    );
    assert_eq!(
        a.node.jobs().limits().slots,
        4,
        "the *configured* size is untouched — the mesh is still told four"
    );

    let mut rx = stream_as(&a, founder_user_key(&a), run_req("echo hello")).await;
    let mut refused = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { .. } => panic!("a 0% node started work"),
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                refused = Some(message);
                break;
            }
            _ => {}
        }
    }
    let message = refused.expect("the run ended in a refusal, not silence");
    assert!(
        !message.is_empty(),
        "a refusal has to say something an operator can act on"
    );
}

/// The refusal is `Busy` — the one the origin already spills on — so a
/// mesh with one quiet node and one loud one still runs the job.
#[tokio::test(flavor = "multi_thread")]
async fn work_spills_from_a_quiet_node_to_a_loud_one() {
    let quiet = spawn_founder_with("quiet", "quiet-spill", vec![], |c| {
        c.limits.slots = 4;
        c.quiet = QuietHours::parse(&covering(0)).expect("window parses");
    })
    .await;
    let loud = mesh_testkit::join_new_node("loud", &quiet, vec![]).await;

    // Converged *and alive*: placement only considers members gossip has
    // seen recently, so waiting on the member count alone races the first
    // gossip round and the job finds nowhere to go for reasons that have
    // nothing to do with quiet hours.
    assert!(
        wait_until(Duration::from_secs(15), || {
            quiet.node.engine().read(|s| s.members.len() == 2)
                && matches!(
                    quiet.node.gossip().liveness_of(&loud.node.node_id()),
                    mesh_proto::Liveness::Alive
                )
        })
        .await,
        "both members converged and the loud one is alive"
    );

    // Submitted *from* the quiet node: a capped node is still a perfectly
    // good origin, which is the half of `no-execute`'s lesson quiet hours
    // inherits (D6) — coordinating work is not running it.
    let mut rx = stream_as(&quiet, founder_user_key(&quiet), run_req("echo spilled")).await;
    let mut executor = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { executor: e, .. } => {
                executor = Some(e);
            }
            IpcResponse::JobDone { .. } => break,
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                panic!("the job should have spilled, not failed: {message}");
            }
            _ => {}
        }
    }
    assert_eq!(
        executor,
        Some(loud.node.node_id()),
        "the job ran on the node that was not quiet"
    );
}

/// A partial cap is a real cap: 25% of four slots is one.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_cap_leaves_a_partial_node() {
    let a = spawn_founder_with("a", "quiet-partial", vec![], |c| {
        c.limits.slots = 4;
        c.quiet = QuietHours::parse(&covering(25)).expect("window parses");
    })
    .await;
    assert_eq!(a.node.jobs().effective_slots(), 1);

    // …and it still runs that one.
    let mut rx = stream_as(&a, founder_user_key(&a), run_req("echo one")).await;
    let mut started = false;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { .. } => started = true,
            IpcResponse::JobDone { .. } => break,
            IpcResponse::Error { message } => panic!("a 25% node refused its one slot: {message}"),
            _ => {}
        }
    }
    assert!(started, "the capped node ran the job it had room for");
}

/// A window that does not cover now changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_elsewhere_in_the_day_is_inert() {
    let a = spawn_founder_with("a", "quiet-inert", vec![], |c| {
        c.limits.slots = 4;
        c.quiet = QuietHours::parse(&avoiding(0)).expect("window parses");
    })
    .await;
    assert_eq!(a.node.jobs().effective_slots(), 4);
    assert!(
        a.node.jobs().quiet_state().is_none(),
        "no window covers now"
    );
}

/// Entering a window stops the *next* job; it never kills the running one.
///
/// This is the property that keeps the gentlest-looking line in the config
/// file from being the most destructive one. A person who wants work to
/// stop is asking for a drain, and that is a different verb.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_opening_never_kills_running_work() {
    let a = spawn_founder_with("a", "quiet-running", vec![], |c| {
        c.limits.slots = 2;
    })
    .await;

    let mut rx = stream_as(
        &a,
        founder_user_key(&a),
        run_req("sleep 3 && echo survived"),
    )
    .await;
    let mut started = false;
    while let Some(frame) = rx.recv().await {
        if matches!(frame, IpcResponse::JobStarted { .. }) {
            started = true;
            break;
        }
    }
    assert!(started, "the job started before the window opened");

    // Now slam the door.
    a.node
        .jobs()
        .set_quiet(QuietHours::parse(&covering(0)).expect("window parses"));
    assert_eq!(a.node.jobs().effective_slots(), 0);
    assert_eq!(
        a.node.jobs().running_count(),
        1,
        "the running job is still running"
    );

    let mut done = None;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::JobDone { state, .. } = frame {
            done = Some(state);
            break;
        }
    }
    assert_eq!(
        done,
        Some(mesh_proto::JobState::Succeeded),
        "the job that was already running ran to completion"
    );
}

/// The cap moves without a restart, in both directions.
#[tokio::test(flavor = "multi_thread")]
async fn the_cap_is_live() {
    let a = spawn_founder_with("a", "quiet-live", vec![], |c| {
        c.limits.slots = 8;
    })
    .await;
    assert_eq!(a.node.jobs().effective_slots(), 8);

    a.node
        .jobs()
        .set_quiet(QuietHours::parse(&covering(50)).expect("window parses"));
    assert_eq!(a.node.jobs().effective_slots(), 4);

    a.node.jobs().set_quiet(QuietHours::default());
    assert_eq!(a.node.jobs().effective_slots(), 8, "and back again");
}

/// `status` reports the configured size *and* what is available, plus the
/// zone the windows are read in — the three facts an operator needs to
/// tell "quiet hours" apart from "the scheduler is broken".
#[tokio::test(flavor = "multi_thread")]
async fn status_says_what_is_capped_and_in_which_zone() {
    let a = spawn_founder_with("a", "quiet-status", vec![], |c| {
        c.limits.slots = 16;
        c.quiet = QuietHours::parse(&covering(25)).expect("window parses");
    })
    .await;

    let IpcResponse::Status(status) = ipc_as(&a, founder_user_key(&a), IpcRequest::Status).await
    else {
        panic!("expected Status");
    };
    assert_eq!(status.slots, 16, "the configured size is still reported");
    let quiet = status.quiet.expect("a node with windows reports them");
    assert_eq!(quiet.effective_slots, 4);
    assert_eq!(quiet.pct, Some(25));
    assert!(quiet.until.is_some(), "and when it ends");
    assert!(!quiet.zone.is_empty(), "and which clock that is on");
    assert!(quiet.windows.contains('@'), "and the window as written");

    // /healthz carries both numbers for the same reason.
    let health = a.node.healthz_json();
    assert!(health.contains("\"slots\":16"), "{health}");
    assert!(health.contains("\"slots_available\":4"), "{health}");
}

/// A node with no windows says nothing about them anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn an_unconfigured_node_is_unchanged() {
    let a = spawn_founder_with("a", "quiet-off", vec![], |c| {
        c.limits.slots = 4;
    })
    .await;
    let IpcResponse::Status(status) = ipc_as(&a, founder_user_key(&a), IpcRequest::Status).await
    else {
        panic!("expected Status");
    };
    assert_eq!(status.quiet, None);
    assert_eq!(a.node.jobs().effective_slots(), 4);
    assert!(a.node.jobs().quiet_next_change().is_none());
}
