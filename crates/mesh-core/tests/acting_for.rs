//! **A node may speak for you only about work you gave it** (SEC-1).
//!
//! A task that calls back into the mesh signs with a per-job credential, and
//! the node running it vouches that the credential speaks for the job's
//! submitter (`mesh_proto::acting`). Before 1.30 that vouching was a node's
//! word: the executor accepted a job id it had never heard of and never
//! asked whether the vouching node had run anything for that user. One
//! compromised member could run commands as the founder on every node.
//!
//! These tests pin both halves. The real path — a job acting for its
//! submitter on another node, and a job started that way doing it again —
//! still works. And a member forging the claim is refused whichever way it
//! tries: with no proof, with a proof it signed itself, with a job summary
//! it made up, or with the founder's own envelope read out of a replicated
//! fan-out plan it was never a target of.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::acting::{ACT_ENV_PREFIX, ActingFor, ActingProof};
use mesh_proto::{
    IpcRequest, IpcResponse, JobId, JobPayload, JobPriority, JobSpec, JobState, JobSummary,
    Liveness, NodeId, OpBody, Request, Response, UserId,
};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, run_to_end, spawn_founder, stream_as,
    wait_until,
};
use tokio::sync::mpsc;

fn run_on(target: &TestNode, command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.to_string(),
        on: Some(format!("name={}", target.config().node_name)),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn schedulable(from: &TestNode, nodes: &[&TestNode]) {
    for node in nodes {
        let id = node.node.node_id();
        assert!(
            wait_until(Duration::from_secs(20), || {
                from.node.engine().read(|s| s.is_active_member(&id))
                    && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
            })
            .await,
            "{} never became schedulable",
            node.config().node_name
        );
    }
}

/// Start `command` on `target` as `user` and leave it running. Returns the
/// job id, the stream (which must be kept alive), and the job's credential.
async fn start_holding(
    origin: &TestNode,
    user: &Keypair,
    target: &TestNode,
    command: &str,
) -> (JobId, mpsc::Receiver<IpcResponse>, Keypair) {
    let mut rx = stream_as(origin, user, run_on(target, command)).await;
    let job = loop {
        match rx
            .recv()
            .await
            .expect("stream ended before the job started")
        {
            IpcResponse::JobStarted { job_id, .. } => break job_id,
            IpcResponse::Notice { .. } => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    };
    let key_path = target
        .node
        .jobs()
        .job_dir_of(&job)
        .join("identity")
        .join("user.key");
    assert!(
        wait_until(Duration::from_secs(20), || key_path.is_file()).await,
        "the job never got its credential"
    );
    let (credential, fresh) = Keypair::load_or_generate(&key_path).unwrap();
    assert!(!fresh, "the credential must be the job's, not a new key");
    (job, rx, credential)
}

fn stdout_of(outcome: RunOutcome) -> String {
    match outcome {
        RunOutcome::Done {
            stdout,
            state: JobState::Succeeded,
            ..
        } => stdout,
        other => panic!("run: {other:?}"),
    }
}

/// The real path, one and two links deep, and the claim stays out of the
/// task's environment.
#[tokio::test]
async fn a_job_acts_for_its_submitter_on_other_nodes() {
    let founder = spawn_founder("NIM", "acting-real").await;
    let a = join_new_node("alpha", &founder, vec![]).await;
    let b = join_new_node("bravo", &founder, vec![]).await;
    schedulable(&founder, &[&a, &b]).await;
    schedulable(&a, &[&founder, &b]).await;
    schedulable(&b, &[&founder, &a]).await;
    let founder_id = founder_user_key(&founder).user_id();

    // A job on alpha, submitted by the founder, holding its credential.
    let (_j0, _rx0, k0) = start_holding(&founder, founder_user_key(&founder), &a, "sleep 60").await;

    // …acts for the founder on bravo: alpha vouches, bravo checks the proof.
    let out = stdout_of(
        run_to_end(
            &a,
            &k0,
            run_on(
                &b,
                &format!("echo acted; env | grep -c '^{ACT_ENV_PREFIX}' || true"),
            ),
        )
        .await,
    );
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("acted"));
    assert_eq!(
        lines.next(),
        Some("0"),
        "the claim is the daemon's business, not the task's"
    );
    // …and on alpha itself, where the credential table is the proof.
    assert_eq!(
        stdout_of(run_to_end(&a, &k0, run_on(&a, "echo here")).await).trim(),
        "here"
    );

    // Two links: a job the credential started on bravo, holding its own
    // credential, acts for the founder on the founder's node. The chain
    // bottoms out in the founder's signature on the first request.
    let (j1, _rx1, k1) = start_holding(&a, &k0, &b, "sleep 60").await;
    let summary = b
        .node
        .engine()
        .read(|s| s.jobs.get(&j1).cloned())
        .expect("bravo knows its own job");
    assert_eq!(
        summary.submitter_user, founder_id,
        "the job is the founder's"
    );
    assert_eq!(
        stdout_of(run_to_end(&b, &k1, run_on(&founder, "echo deeper")).await).trim(),
        "deeper"
    );

    b.stop().await;
    a.stop().await;
    founder.stop().await;
}

/// Send `spec` straight to `target` from `from`, as a member would.
async fn submit_raw(from: &TestNode, target: &TestNode, spec: JobSpec) -> Response {
    let id = target.node.node_id();
    let addrs = from
        .node
        .engine()
        .read(|s| s.members.get(&id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    let conn = from
        .node
        .transport()
        .member_conn(id, &addrs)
        .await
        .expect("dial the target");
    from.node
        .transport()
        .request_one(&conn, &Request::SubmitJob(spec))
        .await
        .expect("a reply")
}

/// A job for `victim` to run as the founder, signed by `key`, vouched for by
/// `rogue` with `claim`.
fn forged_spec(rogue: &TestNode, key: &Keypair, founder_id: UserId, claim: ActingFor) -> JobSpec {
    let command = "echo pwned".to_string();
    let request = IpcRequest::Run {
        command: command.clone(),
        on: None,
        env: vec![],
        timeout_secs: Some(10),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let node_key = rogue.node.node_key_for_test();
    JobSpec {
        job_id: JobId::generate(),
        payload: JobPayload::Command(command),
        env: claim.to_env(|bytes| node_key.sign(bytes)),
        timeout_secs: Some(10),
        priority: JobPriority::Normal,
        submitter: rogue.node.node_id(),
        submitter_user: founder_id,
        auth: key.sign_request(&request).unwrap(),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    }
}

fn claim(key: &Keypair, founder_id: UserId, job: JobId, node: NodeId) -> ActingFor {
    ActingFor {
        key: key.user_id(),
        user: founder_id,
        job,
        node,
        expires_at_micros: Some(mesh_proto::now_micros() + 3_600_000_000),
        proof: None,
    }
}

fn refused(resp: Response, why: &str) {
    match resp {
        Response::Error { message } => assert!(
            message.contains("names a different user") && message.contains(why),
            "refused for the wrong reason — wanted {why:?}, got: {message}"
        ),
        other => panic!("a forged claim was not refused ({why}): {other:?}"),
    }
}

/// Fabricate, on `rogue`, a running job of the founder's that `rogue`
/// claims to be executing, and wait for `victim` to merge it.
async fn fabricate_job(
    rogue: &TestNode,
    victim: &TestNode,
    founder_id: UserId,
    created_at_micros: u64,
    fanout: Option<mesh_proto::FanoutInfo>,
) -> JobId {
    let job = JobId::generate();
    let summary = JobSummary {
        job_id: job,
        cmd_preview: "sleep 600".into(),
        runtime: "bash".into(),
        submitter: rogue.node.node_id(),
        submitter_user: founder_id,
        executor: rogue.node.node_id(),
        state: JobState::Running,
        exit_code: None,
        created_at_micros,
        started_at_micros: Some(created_at_micros),
        finished_at_micros: None,
        fanout,
        updated_hlc: rogue.node.engine().clock().now(),
    };
    rogue
        .node
        .engine()
        .author_op(OpBody::JobUpsert { summary })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || {
            victim.node.engine().read(|s| s.jobs.contains_key(&job))
        })
        .await,
        "the fabricated summary never replicated"
    );
    job
}

#[tokio::test]
async fn a_member_cannot_vouch_for_work_it_was_never_given() {
    let founder = spawn_founder("NIM", "acting-forged").await;
    let rogue = join_new_node("mallory", &founder, vec![]).await;
    let victim = join_new_node("victim", &founder, vec![]).await;
    schedulable(&founder, &[&rogue, &victim]).await;
    schedulable(&rogue, &[&founder, &victim]).await;
    let founder_id = founder_user_key(&founder).user_id();
    let rogue_id = rogue.node.node_id();
    let key = Keypair::generate().unwrap();

    // 1. No proof at all — what every claim was before 1.30.
    let spec = forged_spec(
        &rogue,
        &key,
        founder_id,
        claim(&key, founder_id, JobId::generate(), rogue_id),
    );
    refused(
        submit_raw(&rogue, &victim, spec).await,
        "older than protocol 1.30",
    );

    // 2. A proof the rogue signed itself, citing a job nobody has heard of.
    let self_signed = key.sign_request(&IpcRequest::Jobs).unwrap();
    let mut c = claim(&key, founder_id, JobId::generate(), rogue_id);
    c.proof = Some(ActingProof {
        envelope: self_signed.clone(),
        inner: vec![],
    });
    let spec = forged_spec(&rogue, &key, founder_id, c);
    refused(submit_raw(&rogue, &victim, spec).await, "is not known here");

    // 3. The same, citing a job summary the rogue made up — which it can,
    // because an executor authors its own summaries.
    let job = fabricate_job(&rogue, &victim, founder_id, self_signed.at_micros, None).await;
    let mut c = claim(&key, founder_id, job, rogue_id);
    c.proof = Some(ActingProof {
        envelope: self_signed,
        inner: vec![],
    });
    let spec = forged_spec(&rogue, &key, founder_id, c);
    refused(
        submit_raw(&rogue, &victim, spec).await,
        "signed by neither the user",
    );

    // 4. The founder's own signature, lifted from a fan-out plan that
    // replicated to every member — but that sent work only to the victim.
    let fan = IpcRequest::Run {
        command: "true".into(),
        on: Some(format!("name={}", victim.config().node_name)),
        env: vec![],
        timeout_secs: Some(30),
        fanout: Some(0),
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let mut rx = stream_as(&founder, founder_user_key(&founder), fan).await;
    while rx.recv().await.is_some() {}
    let plan = {
        let mut plan = None;
        assert!(
            wait_until(Duration::from_secs(20), || {
                plan = rogue
                    .node
                    .engine()
                    .read(|s| s.fanouts.values().next().map(|e| e.plan.clone()));
                plan.is_some()
            })
            .await,
            "the fan-out plan never reached the rogue"
        );
        plan.unwrap()
    };
    assert!(plan.targets.iter().all(|t| t.node_id != rogue_id));

    // 4a. …cited by a made-up unplanned job.
    let job = fabricate_job(&rogue, &victim, founder_id, plan.auth.at_micros, None).await;
    let mut c = claim(&key, founder_id, job, rogue_id);
    c.proof = Some(ActingProof {
        envelope: plan.auth.clone(),
        inner: vec![],
    });
    let spec = forged_spec(&rogue, &key, founder_id, c);
    refused(
        submit_raw(&rogue, &victim, spec).await,
        "planned for other jobs",
    );

    // 4b. …cited by a made-up job that claims to belong to the plan.
    let info = mesh_proto::FanoutInfo {
        group: plan.group,
        index: 0,
        total: 1,
    };
    let job = fabricate_job(&rogue, &victim, founder_id, plan.auth.at_micros, Some(info)).await;
    let mut c = claim(&key, founder_id, job, rogue_id);
    c.proof = Some(ActingProof {
        envelope: plan.auth.clone(),
        inner: vec![],
    });
    let spec = forged_spec(&rogue, &key, founder_id, c);
    refused(
        submit_raw(&rogue, &victim, spec).await,
        "does not send it to the node vouching for it",
    );

    // Nothing ran.
    let pwned = victim
        .node
        .engine()
        .read(|s| s.jobs.values().any(|j| j.cmd_preview.contains("pwned")));
    assert!(!pwned, "a forged claim ran work on the victim");

    victim.stop().await;
    rogue.stop().await;
    founder.stop().await;
}
