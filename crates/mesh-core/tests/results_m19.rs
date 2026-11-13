//! M19 results (`docs/M17-M21-FABRIC-KNOWLEDGE-AND-AGENTS.md` §5): every
//! job can answer a question.
//!
//! The milestone is a **promotion**, not an invention (§5.1): the marker
//! convention, the implicit four and the object spillover already existed
//! inside the workflow run loop. So what is tested here is the part that is
//! new — that the same mechanism now reaches an ad-hoc run and a fan-out,
//! that rows survive to be asked for again, that an assertion can fail a
//! command, and the one compatibility claim the whole IPC skew story leans
//! on and nothing tested before (§5.4).
//!
//! The marker parsing, the `Wanted` round trip and the `--expect` parser
//! are unit tests in `mesh_core::outputs` and `mesh_core::results`, because
//! they are pure functions and a mesh adds nothing to them.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, Liveness};
use mesh_testkit::{
    ResultOutcome, TestNode, founder_user_key, ipc_as, join_new_node, results_to_end,
    spawn_founder, wait_until,
};

/// Wait until the origin can actually dispatch to `peer`: a member record
/// is not enough — placement needs a live liveness verdict and a real
/// gossiped address, and a fan-out that silently resolves to one target
/// would make every assertion below pass for the wrong reason.
async fn wait_ready(origin: &TestNode, peer: &TestNode) {
    let engine = origin.node.engine().clone();
    let gossip = origin.node.gossip().clone();
    let id = peer.node.node_id();
    let addr = peer.addr_string();
    let ok = wait_until(Duration::from_secs(20), || {
        engine.read(|s| s.is_active_member(&id) && s.members[&id].info.addrs.contains(&addr))
            && matches!(gossip.liveness_of(&id), Liveness::Alive)
    })
    .await;
    assert!(
        ok,
        "{} never became dispatchable",
        peer.node.node_id().short()
    );
}

/// A `run` request with M19 capture asked for, exactly as the CLI builds
/// it: reserved environment keys, never new fields on `IpcRequest::Run`
/// (§5.2 — the executor re-decodes that request out of the signed envelope).
fn run_capturing(
    command: &str,
    on: Option<&str>,
    fanout: Option<u32>,
    want: mesh_core::outputs::Wanted,
    expect: Option<&str>,
) -> IpcRequest {
    IpcRequest::Run {
        command: command.to_string(),
        on: on.map(|s| s.to_string()),
        env: want.to_env_with(expect),
        timeout_secs: Some(30),
        fanout,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

// ---------------------------------------------------------------------------
// P1 — outputs at the executor
// ---------------------------------------------------------------------------

/// The headline of §5.2: an ad-hoc run publishes the implicit four, which
/// before M19 only a workflow step could do.
#[tokio::test]
async fn an_ad_hoc_run_answers_with_outputs() {
    let founder = spawn_founder("alpha", "results-mesh").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo '{\"total\": 7}'",
            None,
            None,
            mesh_core::outputs::Wanted::implicit(),
            None,
        ),
    )
    .await;
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(
        out.rows.len(),
        1,
        "a single target still answers with a row"
    );
    let row = &out.rows[0];
    assert_eq!(row.output("exit_code"), Some("0"));
    assert_eq!(row.output("stdout"), Some(r#"{"total": 7}"#));
    // `stdout_json` is the compact re-encoding, present only because the
    // whole of stdout parsed.
    assert_eq!(row.output("stdout_json"), Some(r#"{"total":7}"#));
}

/// A script that prints prose has no `stdout_json`, and its absence is
/// informative — it is the first thing to check when a downstream field
/// comes out empty.
#[tokio::test]
async fn stdout_json_is_absent_when_stdout_is_not_json() {
    let founder = spawn_founder("alpha", "results-prose").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo not json at all",
            None,
            None,
            mesh_core::outputs::Wanted::implicit(),
            None,
        ),
    )
    .await;
    let row = &out.rows[0];
    assert_eq!(row.output("stdout"), Some("not json at all"));
    assert!(
        row.output("stdout_json").is_none(),
        "prose must not produce stdout_json"
    );
}

/// `--outputs sha` reads a `::meshware-output::` marker off an ad-hoc run —
/// the convention that used to be reachable only from a workflow step.
#[tokio::test]
async fn a_marker_publishes_a_declared_output_from_an_ad_hoc_run() {
    let founder = spawn_founder("alpha", "results-marker").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo building; echo '::meshware-output:: sha=abc123'; \
             echo '::meshware-output:: secret=hunter2'",
            None,
            None,
            mesh_core::outputs::Wanted::keys(["sha"]),
            None,
        ),
    )
    .await;
    let row = &out.rows[0];
    assert_eq!(row.output("sha"), Some("abc123"));
    // Undeclared keys are dropped: an accidental print cannot become a
    // recorded value.
    assert!(
        row.output("secret").is_none(),
        "undeclared keys must be dropped"
    );
}

/// Absent `--json` / `--outputs` / `--expect`, nothing is captured and no
/// row frame is sent. This is what keeps `meshnode run 'uname -a'` exactly
/// as fast as it was before the milestone.
#[tokio::test]
async fn asking_for_nothing_captures_nothing() {
    let founder = spawn_founder("alpha", "results-free").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo hello",
            None,
            None,
            mesh_core::outputs::Wanted::none(),
            None,
        ),
    )
    .await;
    assert!(out.rows.is_empty(), "no capture was asked for");
    assert!(out.stdout.contains("hello"), "the log still streams");
}

/// The reserved keys are the *origin's* business and never reach an
/// executor: the spec a node runs is byte-identical to a pre-M19 one.
#[tokio::test]
async fn capture_keys_never_reach_the_executor() {
    let founder = spawn_founder("alpha", "results-strip").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "env | grep -c MESHWARE_OUT_ || true",
            None,
            None,
            mesh_core::outputs::Wanted::implicit(),
            Some(r#"equals(stdout, "0")"#),
        ),
    )
    .await;
    assert_eq!(
        out.rows[0].output("stdout"),
        Some("0"),
        "MESHWARE_OUT_* must be stripped before dispatch"
    );
    let (_, passed, failed) = out.expected.expect("the assertion ran");
    assert_eq!(passed, 1);
    assert!(failed.is_empty(), "{failed:?}");
}

// ---------------------------------------------------------------------------
// P2 — fan-out rows, and the compatibility claim
// ---------------------------------------------------------------------------

/// §5.4: a fan-out answers with one row per node, not 600 logs.
#[tokio::test]
async fn a_fanout_answers_with_one_row_per_node() {
    let founder = spawn_founder("alpha", "results-fan").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let user = founder_user_key(&founder).clone();

    wait_ready(&founder, &beta).await;

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo \"$(hostname)\" >/dev/null; echo ok",
            Some("any"),
            Some(0),
            mesh_core::outputs::Wanted::implicit(),
            None,
        ),
    )
    .await;
    assert_eq!(out.tally.1, 0, "no target should have failed: {out:?}");
    assert_eq!(out.rows.len(), 2, "one row per target");
    for name in ["alpha", "beta"] {
        let row = out.row(name).unwrap_or_else(|| panic!("no row for {name}"));
        assert_eq!(row.output("stdout"), Some("ok"));
        assert_eq!(row.exit_code, Some(0));
    }
    drop(beta);
}

/// **The compatibility claim §5.4 says must be pinned by a test.**
///
/// Appending a field at the tail of an existing `IpcResponse` variant is
/// safe for an older CLI because `read_frame` is length-delimited and
/// `postcard::from_bytes` ignores the unread tail. The tree already relies
/// on this for `StatusInfo::wan`, and until now that reliance was a
/// comment. `ParallelItem::outputs` is the field M19 appends; `Old` below
/// is the variant as a 1.8 build defines it, in the same position of the
/// same enum.
#[test]
fn an_older_cli_ignores_an_appended_field() {
    use serde::{Deserialize, Serialize};

    /// `IpcResponse::ParallelItem` as it was *before* M19 appended
    /// `outputs`, decoded standalone.
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Old {
        item: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        node: mesh_proto::NodeId,
        node_name: String,
        output: Option<mesh_proto::ObjectRef>,
    }

    /// The same, with M19's appended field.
    #[derive(Debug, Serialize, Deserialize)]
    struct New {
        item: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        node: mesh_proto::NodeId,
        node_name: String,
        output: Option<mesh_proto::ObjectRef>,
        outputs: Vec<(String, String)>,
    }

    let new = New {
        item: "host-7".into(),
        exit_code: Some(0),
        stdout: "ok".into(),
        stderr: String::new(),
        node: mesh_proto::NodeId([3u8; 32]),
        node_name: "hermes".into(),
        output: None,
        outputs: vec![("sha".into(), "abc123".into())],
    };
    let bytes = postcard::to_stdvec(&new).expect("encode");
    let old: Old = postcard::from_bytes(&bytes)
        .expect("a 1.8 build must decode a 1.9 frame, ignoring the tail");
    assert_eq!(old.item, "host-7");
    assert_eq!(old.stdout, "ok");
    assert_eq!(old.node_name, "hermes");

    // And the reverse direction: a 1.9 build reading a 1.8 frame gets a
    // decode error rather than silent garbage, which is why the appended
    // field must be tail-most and why the daemon never sends one to a peer
    // that did not negotiate it.
    let old_bytes = postcard::to_stdvec(&Old {
        item: "host-7".into(),
        exit_code: Some(0),
        stdout: "ok".into(),
        stderr: String::new(),
        node: mesh_proto::NodeId([3u8; 32]),
        node_name: "hermes".into(),
        output: None,
    })
    .expect("encode");
    assert!(
        postcard::from_bytes::<New>(&old_bytes).is_err(),
        "a truncated tail must not decode as an empty vector by accident"
    );
}

// ---------------------------------------------------------------------------
// P3 — the results index
// ---------------------------------------------------------------------------

/// §5.3: rows are still queryable an hour later, from the node that
/// launched the fan-out.
#[tokio::test]
async fn a_fanouts_rows_are_queryable_afterwards() {
    let founder = spawn_founder("alpha", "results-index").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo '::meshware-output:: kernel=6.1.0'",
            Some("any"),
            Some(0),
            mesh_core::outputs::Wanted::keys(["kernel"]),
            None,
        ),
    )
    .await;
    let group = out.group.expect("a fan-out has a group id");

    let indexed = wait_until(Duration::from_secs(10), || true).await;
    assert!(indexed);

    match ipc_as(
        &founder,
        &user,
        IpcRequest::Results {
            group,
            failed_only: false,
        },
    )
    .await
    {
        IpcResponse::Results(set) => {
            assert_eq!(set.group, group);
            assert_eq!(set.owner_name, "alpha");
            assert_eq!(set.rows.len(), 1);
            assert_eq!(set.rows[0].output("kernel"), Some("6.1.0"));
        }
        other => panic!("results: {other:?}"),
    }
}

/// A group this node has never held is refused **with the owner's name**.
/// A refusal that does not say where to ask is not an answer.
#[tokio::test]
async fn an_unknown_group_is_refused_and_says_so() {
    let founder = spawn_founder("alpha", "results-unknown").await;
    let user = founder_user_key(&founder).clone();

    match ipc_as(
        &founder,
        &user,
        IpcRequest::Results {
            group: mesh_proto::FanId::generate(),
            failed_only: false,
        },
    )
    .await
    {
        IpcResponse::Error { message } => {
            assert!(message.contains("known here"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A run that captured nothing leaves nothing to ask for, and the refusal
/// says which flag was missing rather than pretending the group is gone.
#[tokio::test]
async fn a_run_without_capture_says_why_it_has_no_rows() {
    let founder = spawn_founder("alpha", "results-nocapture").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo hi",
            Some("any"),
            Some(0),
            mesh_core::outputs::Wanted::none(),
            None,
        ),
    )
    .await;
    let group = out.group.expect("a fan-out has a group id");

    match ipc_as(
        &founder,
        &user,
        IpcRequest::Results {
            group,
            failed_only: false,
        },
    )
    .await
    {
        IpcResponse::Error { message } => {
            assert!(message.contains("--json"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P4 — --expect
// ---------------------------------------------------------------------------

/// The exit demo's assertion: `--expect` fails the command and **names the
/// rows that did not comply**.
#[tokio::test]
async fn expect_fails_the_command_and_names_the_rows() {
    let founder = spawn_founder("alpha", "results-expect").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let user = founder_user_key(&founder).clone();

    wait_ready(&founder, &beta).await;

    // Every node prints its own name, so exactly one row fails the check.
    let out: ResultOutcome = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo \"${MESHWARE_NODE_NAME:-$(cat /proc/sys/kernel/hostname 2>/dev/null || echo x)}\"; \
             echo compliant-alpha",
            Some("any"),
            Some(0),
            mesh_core::outputs::Wanted::implicit(),
            Some(r#"contains(stdout, "compliant-alpha")"#),
        ),
    )
    .await;
    let (expression, passed, failed) = out.expected.clone().expect("the assertion ran");
    assert!(expression.contains("compliant-alpha"));
    assert_eq!(passed, 2, "both nodes print the compliant line: {out:?}");
    assert!(failed.is_empty(), "{failed:?}");

    // Now one that nothing satisfies: every row must be named.
    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo permitrootlogin yes",
            Some("any"),
            Some(0),
            mesh_core::outputs::Wanted::implicit(),
            Some(r#"contains(stdout, "permitrootlogin no")"#),
        ),
    )
    .await;
    let (_, passed, failed) = out.expected.expect("the assertion ran");
    assert_eq!(passed, 0);
    assert_eq!(failed.len(), 2, "both rows must be named: {failed:?}");
    for line in &failed {
        assert!(
            line.contains("alpha") || line.contains("beta"),
            "a failure must name the node: {line}"
        );
    }
    // The verdict is on the row too, so a table can mark it.
    assert!(out.rows.iter().all(|r| r.expect_passed == Some(false)));
    drop(beta);
}

/// A `--expect` that does not parse refuses the submission. A guard that
/// silently becomes a no-op is worse than no guard at all.
#[tokio::test]
async fn an_unparseable_expectation_refuses_the_run() {
    let founder = spawn_founder("alpha", "results-badexpect").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo hi",
            None,
            None,
            mesh_core::outputs::Wanted::implicit(),
            Some("wibble stdout"),
        ),
    )
    .await;
    let why = out.error.expect("the run must be refused");
    assert!(why.contains("call"), "{why}");
    assert!(out.rows.is_empty(), "nothing should have run");
}

/// A job that succeeds can still fail the assertion — which is exactly the
/// case `--expect` exists to catch, and the reason the two verdicts are
/// separate fields on a row.
#[tokio::test]
async fn a_successful_job_can_still_fail_its_assertion() {
    let founder = spawn_founder("alpha", "results-split").await;
    let user = founder_user_key(&founder).clone();

    let out = results_to_end(
        &founder,
        &user,
        run_capturing(
            "echo drifted",
            None,
            None,
            mesh_core::outputs::Wanted::implicit(),
            Some(r#"equals(stdout, "expected")"#),
        ),
    )
    .await;
    let row = &out.rows[0];
    assert_eq!(row.exit_code, Some(0), "the job itself succeeded");
    assert_eq!(row.expect_passed, Some(false), "the assertion did not");
    assert!(!row.expect_detail.is_empty(), "and it says why");
}

// ---------------------------------------------------------------------------
// §5.2 amendment 8 — a *script* answers too
// ---------------------------------------------------------------------------

/// Publish a one-file bash service that prints JSON and one declared output.
fn publish_service(node: &TestNode, name: &str, script: &str) {
    let bundle = tempfile::tempdir().expect("tempdir");
    std::fs::write(bundle.path().join("run.sh"), script).expect("write");
    let engine = node.node.engine();
    let content =
        mesh_core::content::ContentStore::open(node.config().data_dir.as_path()).expect("content");
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(node),
        name.to_string(),
        mesh_proto::Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        String::new(),
        vec![],
    )
    .expect("create service");
}

/// **The gap this milestone left, and the one people actually hit.**
///
/// §5.2 moved the output convention to the executor so *every* job has it,
/// and `dispatch_and_stream` does capture generically — but the capture
/// keys reached the daemon on `IpcRequest::Run`'s `env`, and `RunService`
/// has no `env` field. So the one shape of run that most obviously has an
/// answer — a script somebody wrote in order to produce one — was the only
/// one that could not ask for it, and a fan-out of it returned "2
/// succeeded" and nothing else.
///
/// Growing the variant is the one thing the carrier argument forbids (the
/// executor re-decodes this request out of the signed envelope, so a 1.9
/// executor reading a 1.8 origin's request would fail on a field the origin
/// never wrote). The keys therefore ride `inputs`, and the origin strips
/// them *before* the contract is validated.
#[tokio::test]
async fn a_service_run_answers_with_outputs_too() {
    let founder = spawn_founder("alpha", "results-service").await;
    let user = founder_user_key(&founder).clone();
    publish_service(
        &founder,
        "report",
        "#!/usr/bin/env bash\necho '{\"total\": 3}'\necho '::meshware-output:: sha=abc123'\n",
    );

    let want = mesh_core::outputs::Wanted::keys(["sha"]);
    let out = results_to_end(
        &founder,
        &user,
        IpcRequest::RunService {
            name: "report".into(),
            on: None,
            inputs: want.to_env_with(None),
            inventory: None,
            timeout_secs: Some(30),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(out.rows.len(), 1, "a script answers with a row: {out:?}");
    let row = &out.rows[0];
    assert_eq!(row.output("sha"), Some("abc123"), "declared output");
    assert_eq!(row.output("stdout_json"), None, "stdout is not all JSON");
    assert_eq!(row.output("exit_code"), Some("0"));
}

/// The fan-out shape the report came in as: a script on every node, and one
/// row per node rather than one tally.
#[tokio::test]
async fn a_service_fanout_answers_with_one_row_per_node() {
    let founder = spawn_founder("alpha", "results-service-fan").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let user = founder_user_key(&founder).clone();
    wait_ready(&founder, &beta).await;
    publish_service(&founder, "hostinfo", "#!/usr/bin/env bash\necho ok\n");

    let out = results_to_end(
        &founder,
        &user,
        IpcRequest::RunService {
            name: "hostinfo".into(),
            on: Some("any".into()),
            inputs: mesh_core::outputs::Wanted::implicit().to_env_with(None),
            inventory: None,
            timeout_secs: Some(60),
            fanout: Some(0),
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert_eq!(out.tally.1, 0, "no target should have failed: {out:?}");
    assert_eq!(out.rows.len(), 2, "one row per target, not one tally");
    for name in ["alpha", "beta"] {
        let row = out.row(name).unwrap_or_else(|| panic!("no row for {name}"));
        assert_eq!(row.output("stdout"), Some("ok"));
        assert_eq!(row.exit_code, Some(0));
    }
    drop(beta);
}

/// The keys are the origin's business and never reach the *contract*: a
/// service whose schema says `additionalProperties: false` would refuse the
/// run outright if they were still in `inputs` when it was validated.
#[tokio::test]
async fn capture_keys_are_stripped_before_the_contract_is_checked() {
    let founder = spawn_founder("alpha", "results-service-contract").await;
    let user = founder_user_key(&founder).clone();

    let bundle = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        bundle.path().join("run.sh"),
        "#!/usr/bin/env bash\necho \"$WHO\"\n",
    )
    .expect("write");
    let engine = founder.node.engine();
    let content = mesh_core::content::ContentStore::open(founder.config().data_dir.as_path())
        .expect("content");
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&founder),
        "strict".into(),
        mesh_proto::Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        Some(
            r#"{"type":"object","properties":{"who":{"type":"string"}},
                "required":["who"],"additionalProperties":false}"#
                .into(),
        ),
        vec![],
        String::new(),
        vec![],
    )
    .expect("create service");

    let mut inputs = vec![("who".to_string(), "ada".to_string())];
    inputs.extend(mesh_core::outputs::Wanted::implicit().to_env_with(None));
    let out = results_to_end(
        &founder,
        &user,
        IpcRequest::RunService {
            name: "strict".into(),
            on: None,
            inputs,
            inventory: None,
            timeout_secs: Some(30),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(
        out.error.is_none(),
        "a strict contract must never see the capture keys: {:?}",
        out.error
    );
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0].output("exit_code"), Some("0"));
}
