//! ENG-21 — `/metrics` says what it means.
//!
//! Three things were wrong, and all three are the kind that make a
//! dashboard confidently misleading rather than empty:
//!
//! - **`meshware_jobs_finished_total` went down.** It was the number of
//!   terminal job summaries this node still *held*, so the retention sweep
//!   reduced it — and Prometheus reads a falling counter as a reset, which
//!   it turns into a spike of completions that never happened.
//! - **Monotonic series were typed as gauges**, which tells Prometheus that
//!   `rate()` is meaningless on them and that a restart's drop to zero is a
//!   real value.
//! - **Nothing measured the machine or the daemon's own responsiveness**,
//!   which is what a node about to stop working shows first.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, JobId, JobPayload, JobPriority, JobSpec, SignedIpc};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder_with};

/// `# TYPE <name> <kind>` lines, by metric name.
fn types(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

/// The value of a bare (unlabelled) sample.
fn value(text: &str, name: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with(name) && l.as_bytes().get(name.len()) == Some(&b' '))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn labelled(text: &str, name: &str, label: &str) -> Option<f64> {
    with_labels(text, name, &format!("state=\"{label}\""))
}

/// The value of the one sample of `name` whose label set is exactly
/// `labels`.
fn with_labels(text: &str, name: &str, labels: &str) -> Option<f64> {
    let prefix = format!("{name}{{{labels}}}");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn envelope(user: &Keypair, command: &str, nonce: u64) -> SignedIpc {
    let req = IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    let at_micros = mesh_proto::now_micros();
    let signature = user
        .sign(&mesh_proto::ipc_signing_bytes(
            &user.user_id(),
            at_micros,
            nonce,
            &payload,
        ))
        .to_vec();
    SignedIpc {
        user: user.user_id(),
        at_micros,
        nonce,
        payload,
        signature,
    }
}

fn accept(node: &TestNode, command: &str, nonce: u64) -> JobId {
    let user = founder_user_key(node);
    let job_id = JobId::generate();
    node.node
        .jobs()
        .accept(JobSpec {
            job_id,
            payload: JobPayload::Command(command.into()),
            env: vec![],
            timeout_secs: Some(120),
            priority: JobPriority::Normal,
            submitter: node.node.node_id(),
            submitter_user: user.user_id(),
            auth: envelope(user, command, nonce),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        })
        .expect("accept");
    job_id
}

/// The regression itself: what this node has finished is counted as it
/// finishes, and forgetting the record does not un-finish the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_finished_counter_does_not_fall_when_records_are_forgotten() {
    let node = spawn_founder_with("MET", "eng21-counter", vec![], |c| c.limits.slots = 2).await;

    for i in 0..3u64 {
        accept(&node, "true", 800 + i);
    }
    let done = mesh_testkit::wait_until(Duration::from_secs(30), || {
        let jobs = node.node.jobs();
        jobs.running_count() == 0 && jobs.queued_count() == 0
    })
    .await;
    assert!(done, "the jobs never finished");

    let text = node.node.metrics_text();
    let finished = labelled(&text, "meshware_jobs_finished_total", "succeeded")
        .expect("meshware_jobs_finished_total{state=\"succeeded\"}");
    assert!(finished >= 3.0, "counted {finished} of 3 finished jobs");
    assert_eq!(
        value(&text, "meshware_jobs_accepted_total"),
        Some(3.0),
        "every accepted job should be counted once"
    );

    let held = labelled(&text, "meshware_jobs_recorded", "succeeded").unwrap_or(0.0);
    assert!(held >= 3.0, "the node should be holding 3 records: {held}");

    // Forget every record — what a retention sweep does. The gauge that
    // describes what is *held* falls; the counter must not.
    let user = founder_user_key(&node);
    let watermark = mesh_proto::now_micros() + 1_000_000;
    node.node
        .engine()
        .author_op(mesh_proto::OpBody::JobsClear {
            before_micros: watermark,
            by: user.user_id(),
            signature: user
                .sign(&mesh_proto::jobs_clear_signing_bytes(
                    watermark,
                    &user.user_id(),
                ))
                .to_vec(),
        })
        .expect("clear");

    let after = node.node.metrics_text();
    // The premise: the records really did go. Without this the rest of the
    // test passes whether or not the counter was ever fixed.
    let held_after = labelled(&after, "meshware_jobs_recorded", "succeeded").unwrap_or(0.0);
    assert!(
        held_after < held,
        "the clear should have forgotten records: {held} then {held_after}"
    );

    let still = labelled(&after, "meshware_jobs_finished_total", "succeeded").unwrap_or(0.0);
    assert!(
        still >= finished,
        "a counter must never fall when records are forgotten: {finished} then {still}"
    );

    node.stop().await;
}

/// Every series that only goes up is typed as a counter, and every series
/// that describes a current quantity is typed as a gauge. A `_total` typed
/// as a gauge tells Prometheus that `rate()` on it is meaningless.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monotonic_series_are_counters_and_snapshots_are_gauges() {
    let node = spawn_founder_with("TYPES", "eng21-types", vec![], |_| {}).await;
    let text = node.node.metrics_text();
    let types = types(&text);

    for name in [
        "meshware_jobs_finished_total",
        "meshware_jobs_accepted_total",
        "meshware_jobs_started_total",
        "meshware_jobs_refused_total",
        "meshware_sessions_opened_total",
        "meshware_sessions_refused_total",
        "meshware_session_bytes_total",
        "meshware_dispatch_failures_total",
        "meshware_dispatch_spills_total",
    ] {
        assert_eq!(
            types.get(name).map(String::as_str),
            Some("counter"),
            "{name} only ever goes up and must be typed as a counter"
        );
    }

    for name in [
        "meshware_jobs_running",
        "meshware_jobs_queued",
        "meshware_jobs_recorded",
        "meshware_executor_slots",
        "meshware_members_alive",
        "meshware_members_unreachable",
        // Counts of what exists now, whatever the name suggests.
        "meshware_members_total",
    ] {
        assert_eq!(
            types.get(name).map(String::as_str),
            Some("gauge"),
            "{name} describes a quantity now and must be typed as a gauge"
        );
    }

    node.stop().await;
}

/// The series that were missing entirely: the daemon's own responsiveness,
/// and the machine it is about to run out of.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_and_the_machine_are_both_measured() {
    let node = spawn_founder_with("MACHINE", "eng21-machine", vec![], |_| {}).await;
    // One request, so the IPC histogram has something in it.
    let _ = mesh_testkit::ipc_as(&node, founder_user_key(&node), IpcRequest::Jobs).await;

    let text = node.node.metrics_text();
    let types = types(&text);

    assert_eq!(
        types.get("meshware_ipc_seconds").map(String::as_str),
        Some("histogram"),
        "nothing measured whether the daemon answers at all"
    );
    assert!(
        value(&text, "meshware_ipc_seconds_count").is_some_and(|n| n >= 1.0),
        "the request this test made should have been timed"
    );
    for name in [
        "meshware_job_queue_wait_seconds",
        "meshware_job_duration_seconds",
    ] {
        assert_eq!(
            types.get(name).map(String::as_str),
            Some("histogram"),
            "{name}"
        );
    }

    for name in [
        "meshware_disk_free_bytes",
        "meshware_disk_total_bytes",
        "meshware_oplog_store_bytes",
        "process_resident_memory_bytes",
        "process_open_fds",
    ] {
        assert!(
            value(&text, name).is_some_and(|v| v > 0.0),
            "{name} is missing or zero"
        );
    }

    node.stop().await;
}

/// Prometheus refuses a body whose HELP/TYPE do not precede their samples,
/// and silently drops a duplicate series. Both are easy to introduce by
/// adding a metric in the wrong place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_exposition_is_well_formed() {
    let node = spawn_founder_with("FORM", "eng21-form", vec![], |_| {}).await;
    let text = node.node.metrics_text();

    let mut declared: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            assert!(
                !declared.contains(&name),
                "{name} is declared twice; Prometheus keeps one and drops the rest"
            );
            declared.push(name);
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let name = line
            .split(['{', ' '])
            .next()
            .unwrap_or("")
            // Histogram samples are `<name>_bucket` / `_sum` / `_count` of
            // a series declared under `<name>`.
            .trim_end_matches("_bucket")
            .trim_end_matches("_sum")
            .trim_end_matches("_count");
        assert!(
            declared.contains(&name),
            "sample {line:?} has no preceding # TYPE"
        );
        // Every value must parse as a float, or the whole scrape is refused.
        let raw = line.split_whitespace().next_back().unwrap_or("");
        assert!(
            raw.parse::<f64>().is_ok(),
            "sample {line:?} does not end in a number"
        );
    }
    assert!(declared.len() > 20, "only {} series", declared.len());

    node.stop().await;
}

/// The rest of what ENG-21 said was missing: the three ways one submission
/// becomes many jobs, the mesh as this node sees it, ops that cannot be
/// applied yet, and what the limiters turned away.
///
/// Every one of these is published at zero on an idle node on purpose. A
/// series that appears only once it is non-zero cannot be alerted on
/// before the first incident, which is the one it would have been useful
/// for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_series_that_were_missing_are_present_from_the_first_scrape() {
    let node = spawn_founder_with("MISSING", "eng21-missing", vec![], |_| {}).await;
    let text = node.node.metrics_text();
    let types = types(&text);

    for name in [
        "meshware_group_runs_started_total",
        "meshware_group_runs_finished_total",
        "meshware_group_subjobs_total",
        "meshware_refusals_total",
        "meshware_op_rate_pauses_total",
    ] {
        assert_eq!(
            types.get(name).map(String::as_str),
            Some("counter"),
            "{name} only goes up and must be typed as a counter"
        );
    }
    assert_eq!(
        types.get("meshware_group_run_seconds").map(String::as_str),
        Some("histogram")
    );
    for name in ["meshware_ops_parked", "meshware_peer_alive"] {
        assert_eq!(
            types.get(name).map(String::as_str),
            Some("gauge"),
            "{name} describes a quantity now and must be typed as a gauge"
        );
    }

    // All three kinds, all three outcomes, from the first scrape.
    for kind in ["fanout", "parallel", "gang"] {
        assert_eq!(
            with_labels(
                &text,
                "meshware_group_runs_started_total",
                &format!("kind=\"{kind}\"")
            ),
            Some(0.0),
            "{kind} runs are not published on an idle node"
        );
        for outcome in ["completed", "failed", "halted"] {
            assert_eq!(
                with_labels(
                    &text,
                    "meshware_group_runs_finished_total",
                    &format!("kind=\"{kind}\",outcome=\"{outcome}\"")
                ),
                Some(0.0),
                "{kind}/{outcome} is missing"
            );
        }
    }

    // Every limiter on the node, named. `op_rate` is the one that decides
    // whether this node's own log grows without bound; the other four sit
    // in front of doors open to anybody holding a key (SEC-19).
    for limit in [
        "op_rate",
        "enrol_source",
        "enrol_all",
        "login_source",
        "login_all",
    ] {
        assert_eq!(
            with_labels(
                &text,
                "meshware_refusals_total",
                &format!("limit=\"{limit}\"")
            ),
            Some(0.0),
            "the {limit} limiter is not published"
        );
    }

    assert_eq!(value(&text, "meshware_ops_parked"), Some(0.0));

    node.stop().await;
}

/// A group run this node owns is counted on this node, even though this
/// node executes none of it.
///
/// A single-member mesh is still a fan-out of one: the plan replicates,
/// the group is driven, and it closes — which is every step the meter
/// hangs off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fanout_is_counted_by_the_node_that_owns_it() {
    let node = spawn_founder_with("FAN", "eng21-fan", vec![], |c| c.limits.slots = 2).await;
    let user = founder_user_key(&node);

    let before = node.node.metrics_text();
    assert_eq!(
        with_labels(
            &before,
            "meshware_group_runs_started_total",
            "kind=\"fanout\""
        ),
        Some(0.0),
        "the premise: nothing has run yet"
    );

    let mut frames = mesh_testkit::stream_as(
        &node,
        user,
        IpcRequest::Run {
            command: "true".into(),
            on: None,
            env: vec![],
            timeout_secs: Some(120),
            fanout: Some(0), // every matching node — here, this one
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    let mut done = false;
    let mut seen: Vec<String> = Vec::new();
    while let Some(frame) = frames.recv().await {
        seen.push(format!("{frame:?}").chars().take(120).collect());
        if matches!(frame, mesh_proto::IpcResponse::FanoutDone { .. }) {
            done = true;
        }
    }
    assert!(done, "the fan-out never reported finishing; saw {seen:#?}");

    let after = node.node.metrics_text();
    assert_eq!(
        with_labels(
            &after,
            "meshware_group_runs_started_total",
            "kind=\"fanout\""
        ),
        Some(1.0),
        "the owner did not count the run"
    );
    assert_eq!(
        with_labels(
            &after,
            "meshware_group_runs_finished_total",
            "kind=\"fanout\",outcome=\"completed\""
        ),
        Some(1.0),
        "…or did not count it finishing"
    );
    assert_eq!(
        with_labels(
            &after,
            "meshware_group_subjobs_total",
            "kind=\"fanout\",outcome=\"succeeded\""
        ),
        Some(1.0)
    );
    // The duration has to land in a bucket. On the latency ladder every
    // group run lands in `+Inf` and the histogram reports "slower than ten
    // seconds" for all of them, which is true and useless.
    assert_eq!(
        with_labels(
            &after,
            "meshware_group_run_seconds_count",
            "kind=\"fanout\""
        ),
        Some(1.0)
    );
    let in_a_bucket = with_labels(
        &after,
        "meshware_group_run_seconds_bucket",
        "kind=\"fanout\",le=\"60\"",
    );
    assert_eq!(
        in_a_bucket,
        Some(1.0),
        "a `true` across one node should be well under a minute"
    );

    node.stop().await;
}
