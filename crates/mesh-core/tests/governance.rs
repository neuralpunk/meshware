//! M3 exit criteria (MASTERPLAN §17): two users with different grants; an
//! auditor reproduces exactly who ran what; a secret never appears in logs.
//! Plus: merge-time rejection of smuggled privileges, secret file injection
//! + shred, provider plugins, rewrap for late joiners, replay rejection.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_core::secrets::seal_secret;
use mesh_core::state::StateEngine;
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, JobId, JobState, Liveness, MemberInfo, NodeId, OpBody,
    OpEnvelope, Role, UserId, UserRecord,
};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, join_new_node, run_to_end, spawn_founder,
    spawn_founder_with, stream_as, wait_until,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn signed_user_record(admin: &Keypair, user: &Keypair, username: &str) -> UserRecord {
    let mut record = UserRecord {
        user_id: user.user_id(),
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    record
}

fn signed_grant(
    admin: &Keypair,
    user_id: UserId,
    role: Role,
    nodes: &str,
    services: &str,
) -> Grant {
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id,
        role,
        nodes: nodes.into(),
        services: services.into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    grant
}

/// Register a user + one grant through the founder's signed IPC path.
async fn add_user(
    node: &TestNode,
    user: &Keypair,
    username: &str,
    role: Role,
    nodes: &str,
    services: &str,
) {
    let admin = founder_user_key(node);
    let record = signed_user_record(admin, user, username);
    let resp = ipc_as(node, admin, IpcRequest::UserAdd { record }).await;
    assert!(matches!(resp, IpcResponse::Ok), "user add: {resp:?}");
    let grant = signed_grant(admin, user.user_id(), role, nodes, services);
    let resp = ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await;
    assert!(matches!(resp, IpcResponse::Ok), "grant add: {resp:?}");
}

/// Seal + publish a secret as the founding admin, wrapped for all current
/// active members.
async fn set_secret(node: &TestNode, name: &str, value: &[u8]) {
    let admin = founder_user_key(node);
    let recipients: Vec<NodeId> = node.node.engine().read(|s| {
        s.members
            .values()
            .filter(|m| s.is_active_member(&m.info.node_id))
            .map(|m| m.info.node_id)
            .collect()
    });
    let mut record = seal_secret(name, 1, value, &recipients, admin.user_id()).unwrap();
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::SecretPublish { record }).await;
    assert!(matches!(resp, IpcResponse::Ok), "secret publish: {resp:?}");
}

fn run_req(command: &str, on: Option<&str>, env: Vec<(String, String)>) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: on.map(str::to_string),
        env,
        timeout_secs: Some(60),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

fn job_dir(node: &TestNode, job_id: JobId) -> PathBuf {
    node.config()
        .data_dir
        .join("jobs")
        .join(job_id.to_display_string())
}

async fn wait_beta_alive(a: &TestNode, b: &TestNode) {
    let engine = a.node.engine().clone();
    let gossip = a.node.gossip().clone();
    let beta_id = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine.read(|s| s.is_active_member(&beta_id))
                && matches!(gossip.liveness_of(&beta_id), Liveness::Alive)
        })
        .await,
        "founder should see the joined node alive"
    );
}

/// Wait until `node`'s state knows the user and at least one grant for them.
async fn wait_user_known(node: &TestNode, user: UserId) {
    let engine = node.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine.read(|s| {
                s.users.contains_key(&user) && s.grants.values().any(|g| g.grant.user_id == user)
            })
        })
        .await,
        "user + grant should replicate"
    );
}

// ---------------------------------------------------------------------------
// two users, different grants (the M3 exit demo, part 1)
// ---------------------------------------------------------------------------

/// Cancel is scoped by role *and* ownership — admins and operators end
/// anything, a developer only their own jobs — and every refusal names the
/// rule and who can act, because "denied" without "who to ask" is how a
/// stuck job stays stuck.
#[tokio::test]
async fn cancel_is_scoped_and_refusals_name_the_rule() {
    let a = spawn_founder("alpha", "hyacinth").await;

    let dana = Keypair::generate().unwrap(); // developer, owns the job
    let devon = Keypair::generate().unwrap(); // developer, does not
    let olive = Keypair::generate().unwrap(); // operator
    let vera = Keypair::generate().unwrap(); // viewer
    add_user(&a, &dana, "dana", Role::Developer, "any", "*").await;
    add_user(&a, &devon, "devon", Role::Developer, "any", "*").await;
    add_user(&a, &olive, "olive", Role::Operator, "any", "*").await;
    add_user(&a, &vera, "vera", Role::Viewer, "any", "*").await;

    // Dana launches something long-lived; the stream stays bound so the
    // job is still running for everyone below to be refused over.
    let mut rx = stream_as(&a, &dana, run_req("sleep 30", None, vec![])).await;
    let job_id = loop {
        match rx.recv().await.expect("stream should not end yet") {
            IpcResponse::JobStarted { job_id, .. } => break job_id,
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                panic!("dana's run refused: {message}")
            }
            _ => {}
        }
    };

    // Another developer may not end it, and the refusal says whose job it
    // is and which roles can — not a bare "no".
    match ipc_as(&a, &devon, IpcRequest::JobCancel { job_id }).await {
        IpcResponse::Denied { message } => {
            assert!(message.contains("their own jobs"), "the rule: {message}");
            assert!(message.contains("dana"), "the owner, by name: {message}");
            assert!(message.contains("operator or admin"), "who can: {message}");
        }
        other => panic!("devon must be refused: {other:?}"),
    }

    // A viewer is told what cancelling needs and what they hold.
    match ipc_as(&a, &vera, IpcRequest::JobCancel { job_id }).await {
        IpcResponse::Denied { message } => {
            assert!(
                message.contains("needs developer"),
                "what would work: {message}"
            );
            assert!(message.contains("viewer"), "what they hold: {message}");
        }
        other => panic!("vera must be refused: {other:?}"),
    }

    // The job is still running — a refusal that killed it anyway would
    // make every assertion above decorative.
    let engine = a.node.engine().clone();
    assert!(
        engine.read(|s| s.jobs.get(&job_id).is_some_and(|j| !j.state.is_terminal())),
        "refused cancels must not touch the job"
    );

    // An operator ends anybody's job.
    match ipc_as(&a, &olive, IpcRequest::JobCancel { job_id }).await {
        IpcResponse::Ok => {}
        other => panic!("olive (operator) should cancel any job: {other:?}"),
    }

    // Dana's own next job is hers to end — and through the force verb the
    // Director sends, the *ordinary* path fires while the executor is
    // reachable, so no operator power is demanded of her.
    let _rx2 = stream_as(&a, &dana, run_req("sleep 30", None, vec![])).await;
    let jobs = a.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(10), || {
            jobs.read(|s| {
                s.jobs
                    .values()
                    .any(|j| j.state == JobState::Running && j.job_id != job_id)
            })
        })
        .await,
        "dana's second job should start"
    );
    let second = a
        .node
        .engine()
        .read(|s| {
            s.jobs
                .values()
                .find(|j| j.state == JobState::Running && j.job_id != job_id)
                .map(|j| j.job_id)
        })
        .expect("second job");
    let by = dana.user_id();
    let signature = dana
        .sign(&mesh_proto::job_cancel_force_signing_bytes(&second, &by))
        .to_vec();
    match ipc_as(
        &a,
        &dana,
        IpcRequest::JobCancelForce {
            job_id: second,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { message } => assert!(
            message.contains("cancel requested"),
            "reachable executor means the ordinary route, not a force: {message}"
        ),
        other => panic!("dana cancelling her own job must work: {other:?}"),
    }

    a.node.shutdown();
}

#[tokio::test]
async fn two_users_with_different_grants() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("web-1", &a, vec!["web".into()]).await;
    wait_beta_alive(&a, &b).await;

    let alice = Keypair::generate().unwrap(); // developer, everywhere
    let bob = Keypair::generate().unwrap(); // operator, web nodes only
    add_user(&a, &alice, "alice", Role::Developer, "any", "*").await;
    add_user(&a, &bob, "bob", Role::Operator, "tag=web", "*").await;
    wait_user_known(&b, bob.user_id()).await;

    // Alice may run anywhere.
    match run_to_end(&a, &alice, run_req("echo alice-was-here", None, vec![])).await {
        RunOutcome::Done { state, stdout, .. } => {
            assert_eq!(state, JobState::Succeeded, "stdout: {stdout}");
        }
        other => panic!("alice should run: {other:?}"),
    }

    // Bob may run on web-tagged nodes…
    match run_to_end(
        &b,
        &bob,
        run_req("echo bob-on-web", Some("tag=web"), vec![]),
    )
    .await
    {
        RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
        other => panic!("bob should run on web: {other:?}"),
    }
    // …but nowhere else: alpha has no `web` tag, so bob's grant covers no
    // matching node.
    match run_to_end(
        &a,
        &bob,
        run_req("echo bob-anywhere", Some("name=alpha"), vec![]),
    )
    .await
    {
        RunOutcome::Denied(_) => {}
        other => panic!("bob must be denied off web nodes: {other:?}"),
    }

    // Bob (operator) cannot register services; the request is denied at the
    // role gate before anything is authored.
    let version = mesh_core::services::sign_service_version(
        &bob,
        mesh_proto::ServiceVersion {
            name: "sneaky".into(),
            version: 1,
            runtime: mesh_proto::Runtime::Bash,
            bundle_id: mesh_proto::BundleId([0u8; 32]),
            working_dir: String::new(),
            entrypoint: "x.sh".into(),
            contract: None,
            env: vec![],
            description: String::new(),
            tags: vec![],
            created_at_micros: mesh_proto::now_micros(),
            created_by: bob.user_id(),
            source: None,
            signature: Vec::new(),
        },
    );
    let head_signature = mesh_core::services::sign_service_head(&bob, "sneaky", 1);
    let resp = ipc_as(
        &a,
        &bob,
        IpcRequest::ServiceCreate {
            version: Box::new(version),
            head_signature,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Denied { .. }), "got {resp:?}");

    // A complete stranger (valid signature, unknown key) is denied.
    let mallory = Keypair::generate().unwrap();
    match run_to_end(&a, &mallory, run_req("echo intruder", None, vec![])).await {
        RunOutcome::Denied(msg) => assert!(msg.contains("unknown"), "msg: {msg}"),
        other => panic!("stranger must be denied: {other:?}"),
    }

    a.node.shutdown();
    b.node.shutdown();
}

// ---------------------------------------------------------------------------
// merge-time authorization: a compromised node cannot smuggle privileges
// ---------------------------------------------------------------------------

fn craft_op(author: &Keypair, seq: u64, body: OpBody) -> OpEnvelope {
    mesh_testkit::ops::seal(author, seq, mesh_testkit::ops::now_hlc(), &body)
}

fn member_info(key: &Keypair, name: &str) -> MemberInfo {
    MemberInfo {
        node_id: key.node_id(),
        name: name.into(),
        addrs: vec!["127.0.0.1:7777".into()],
        tags: vec![],
        slots: 4,
        joined_at_micros: mesh_proto::now_micros(),
    }
}

#[tokio::test]
async fn compromised_node_cannot_smuggle_privileges() {
    let dir = tempfile::tempdir().unwrap();
    let founder_user = Keypair::generate().unwrap();
    let founder_node = Keypair::generate().unwrap();
    let rogue_node = Keypair::generate().unwrap();
    let mallory = Keypair::generate().unwrap();
    let genesis =
        mesh_core::genesis::Genesis::create("m", &founder_user, founder_node.node_id()).unwrap();
    let engine = StateEngine::open(dir.path(), founder_node.clone(), genesis).unwrap();

    // Legitimate history: founder self-adds, admits the (soon rogue) node,
    // registers mallory as a viewer.
    engine
        .author_op(OpBody::MemberAdd {
            member: member_info(&founder_node, "alpha"),
            invite: None,
        })
        .unwrap();
    engine
        .author_op(OpBody::MemberAdd {
            member: member_info(&rogue_node, "rogue"),
            invite: None,
        })
        .unwrap();
    engine
        .author_op(OpBody::UserAdd {
            record: signed_user_record(&founder_user, &mallory, "mallory"),
        })
        .unwrap();
    engine
        .author_op(OpBody::GrantAdd {
            grant: signed_grant(&founder_user, mallory.user_id(), Role::Viewer, "any", "*"),
        })
        .unwrap();

    // The rogue node (a real, active member!) tries to escalate mallory to
    // admin. The grant is signed by mallory herself — a valid signature,
    // but not an admin's — so every honest node drops it at merge time.
    let escalation = signed_grant(&mallory, mallory.user_id(), Role::Admin, "any", "*");
    let op = craft_op(&rogue_node, 1, OpBody::GrantAdd { grant: escalation });
    engine.ingest(vec![op]).unwrap();
    let founder_pub = founder_user.public_bytes();
    assert!(
        !engine.read(|s| mesh_core::rbac::has_role(
            s,
            &founder_pub,
            &mallory.user_id(),
            &[Role::Admin],
            mesh_proto::now_micros(),
        )),
        "mallory must not become admin via a compromised node"
    );

    // Same for a forged service definition signed by a non-developer.
    let version = mesh_core::services::sign_service_version(
        &mallory,
        mesh_proto::ServiceVersion {
            name: "backdoor".into(),
            version: 1,
            runtime: mesh_proto::Runtime::Bash,
            bundle_id: mesh_proto::BundleId([7u8; 32]),
            working_dir: String::new(),
            entrypoint: "b.sh".into(),
            contract: None,
            env: vec![],
            description: String::new(),
            tags: vec![],
            created_at_micros: mesh_proto::now_micros(),
            created_by: mallory.user_id(),
            source: None,
            signature: Vec::new(),
        },
    );
    let op = craft_op(&rogue_node, 2, OpBody::ServiceVersionAdd { version });
    engine.ingest(vec![op]).unwrap();
    assert!(engine.read(|s| !s.services.contains_key("backdoor")));

    // And a grant whose signature does not verify at all (claimed founder,
    // signed by mallory) is dropped as forged.
    let mut forged = signed_grant(&mallory, mallory.user_id(), Role::Admin, "any", "*");
    forged.granted_by = founder_user.user_id();
    let op = craft_op(&rogue_node, 3, OpBody::GrantAdd { grant: forged });
    engine.ingest(vec![op]).unwrap();
    assert!(!engine.read(|s| mesh_core::rbac::has_role(
        s,
        &founder_pub,
        &mallory.user_id(),
        &[Role::Admin],
        mesh_proto::now_micros(),
    )));
}

// ---------------------------------------------------------------------------
// secrets: never in logs, never in job records (exit demo, part 3)
// ---------------------------------------------------------------------------

const SECRET_VALUE: &str = "swordfish-Xk9#q2";

#[tokio::test]
async fn secret_value_never_appears_in_logs_or_records() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("worker-1", &a, vec!["worker".into()]).await;
    wait_beta_alive(&a, &b).await;

    set_secret(&a, "db-pass", SECRET_VALUE.as_bytes()).await;

    // Wait for the secret to replicate to the executor.
    let engine_b = b.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_b.read(|s| s.secrets.contains_key("db-pass"))
        })
        .await
    );

    // Run on the remote executor with a late-binding reference; the job
    // prints the secret — which must reach neither the stream nor the spool.
    let admin = founder_user_key(&a);
    let outcome = run_to_end(
        &a,
        admin,
        run_req(
            "echo \"the password is $DB_PASS\"; echo done",
            Some("tag=worker"),
            vec![("DB_PASS".into(), "{{secret:db-pass}}".into())],
        ),
    )
    .await;
    let RunOutcome::Done {
        stdout,
        state,
        job_id,
        ..
    } = outcome
    else {
        panic!("run failed: {outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded, "stdout: {stdout}");
    let job_id = job_id.unwrap();

    // Streamed output is redacted…
    assert!(!stdout.contains(SECRET_VALUE), "stream leaked: {stdout}");
    assert!(stdout.contains("[secret:db-pass]"), "stdout: {stdout}");

    // …the executor's spooled log is redacted…
    let dir = job_dir(&b, job_id);
    let spooled = std::fs::read_to_string(dir.join("stdout.log")).unwrap();
    assert!(!spooled.contains(SECRET_VALUE), "spool leaked: {spooled}");
    assert!(spooled.contains("[secret:db-pass]"));

    // …and the persisted job record holds only the reference.
    let spec = std::fs::read_to_string(dir.join("spec.json")).unwrap();
    assert!(!spec.contains(SECRET_VALUE), "spec leaked");
    assert!(spec.contains("{{secret:db-pass}}"));

    // Secret refs in the command line itself are refused (argv is public).
    match run_to_end(&a, admin, run_req("echo {{secret:db-pass}}", None, vec![])).await {
        RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Failed),
        RunOutcome::Error(_) => {}
        other => panic!("unexpected: {other:?}"),
    }

    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn secret_file_injection_is_0400_and_shredded() {
    let a = spawn_founder("alpha", "hyacinth").await;
    set_secret(&a, "tls-key", SECRET_VALUE.as_bytes()).await;

    let admin = founder_user_key(&a);
    // The job validates mode 0400 and the exact byte length from inside.
    let script = format!(
        "[ \"$(stat -c %a \"$KEY_FILE\")\" = 400 ] && [ \"$(wc -c < \"$KEY_FILE\")\" -eq {} ]",
        SECRET_VALUE.len()
    );
    let outcome = run_to_end(
        &a,
        admin,
        run_req(
            &script,
            None,
            vec![("KEY_FILE".into(), "{{secret_file:tls-key}}".into())],
        ),
    )
    .await;
    let RunOutcome::Done { state, job_id, .. } = outcome else {
        panic!("run failed: {outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded, "file mode/content check failed");

    // After the job, the injected file is gone.
    let secrets_dir = job_dir(&a, job_id.unwrap()).join("secrets");
    assert!(!secrets_dir.exists(), "secret files must be shredded");

    a.node.shutdown();
}

// ---------------------------------------------------------------------------
// provider plugins (the adopted contract)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_plugin_resolves_references() {
    // A plugin that honors the contract: `plugin get`, JSON on stdin,
    // {"value": …} on stdout.
    let plugin_dir = tempfile::tempdir().unwrap();
    let plugin = plugin_dir.path().join("dummy-provider");
    std::fs::write(
        &plugin,
        "#!/usr/bin/env bash\n[ \"$1\" = get ] || exit 2\ncat > /dev/null\necho '{\"value\":\"from-provider-9000\"}'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let plugin_path = plugin.to_string_lossy().into_owned();

    let a = spawn_founder_with("alpha", "hyacinth", vec![], |cfg| {
        cfg.secret_providers.insert(
            "dummy".into(),
            mesh_core::config::ProviderConfig {
                plugin: plugin_path,
                config: serde_json::json!({"region": "test"}),
            },
        );
    })
    .await;

    let admin = founder_user_key(&a);
    // Assert the value arrived (by length) and that echoing it is redacted.
    let outcome = run_to_end(
        &a,
        admin,
        run_req(
            "echo \"got: $PW\"; [ \"${#PW}\" -eq 18 ]",
            None,
            vec![("PW".into(), "{{secret:dummy:kv/app#token}}".into())],
        ),
    )
    .await;
    let RunOutcome::Done { state, stdout, .. } = outcome else {
        panic!("run failed: {outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded, "stdout: {stdout}");
    assert!(!stdout.contains("from-provider-9000"), "leaked: {stdout}");
    assert!(
        stdout.contains("[secret:dummy:kv/app#token]"),
        "stdout: {stdout}"
    );

    a.node.shutdown();
}

// ---------------------------------------------------------------------------
// rewrap: nodes that join after sealing can still decrypt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rewrap_heals_late_joiners() {
    let a = spawn_founder("alpha", "hyacinth").await;
    // Sealed while the founder is alone: wrapped for one node only.
    set_secret(&a, "late-secret", b"late-but-great").await;

    let b = join_new_node("late-1", &a, vec!["late".into()]).await;
    wait_beta_alive(&a, &b).await;

    // The founder holds a wrap, notices the newcomer, and heals it.
    a.node.rewrap_secrets();
    let engine_b = b.node.engine().clone();
    let beta_id = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_b.read(|s| {
                s.secrets
                    .get("late-secret")
                    .is_some_and(|e| e.record.wraps.iter().any(|w| w.node_id == beta_id))
            })
        })
        .await,
        "rewrap should reach the late joiner"
    );

    // And the late joiner can now execute work that needs the secret.
    let admin = founder_user_key(&a);
    let outcome = run_to_end(
        &a,
        admin,
        run_req(
            "[ \"${#S}\" -eq 14 ]",
            Some("tag=late"),
            vec![("S".into(), "{{secret:late-secret}}".into())],
        ),
    )
    .await;
    let RunOutcome::Done { state, .. } = outcome else {
        panic!("run failed: {outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded);

    a.node.shutdown();
    b.node.shutdown();
}

// ---------------------------------------------------------------------------
// audit: who ran what (exit demo, part 2) + replay protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auditor_reproduces_who_ran_what() {
    let a = spawn_founder("alpha", "hyacinth").await;

    let alice = Keypair::generate().unwrap();
    let carol = Keypair::generate().unwrap(); // auditor
    let vera = Keypair::generate().unwrap(); // viewer
    add_user(&a, &alice, "alice", Role::Operator, "any", "*").await;
    add_user(&a, &carol, "carol", Role::Auditor, "any", "*").await;
    add_user(&a, &vera, "vera", Role::Viewer, "any", "*").await;

    set_secret(&a, "api-token", SECRET_VALUE.as_bytes()).await;

    // Alice runs a job that touches the secret.
    let outcome = run_to_end(
        &a,
        &alice,
        run_req(
            "echo using token",
            None,
            vec![("T".into(), "{{secret:api-token}}".into())],
        ),
    )
    .await;
    let RunOutcome::Done { state, job_id, .. } = outcome else {
        panic!("alice's run failed: {outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded);
    let job_id = job_id.unwrap();

    // Vera (viewer) tries to run — denied, and the denial is recorded.
    match run_to_end(&a, &vera, run_req("echo nope", None, vec![])).await {
        RunOutcome::Denied(_) => {}
        other => panic!("viewer must not run: {other:?}"),
    }

    // Carol the auditor reproduces the story. Auditors read everything…
    let resp = ipc_as(
        &a,
        &carol,
        IpcRequest::AuditQuery {
            user: None,
            action: None,
            since_micros: None,
            limit: 100,
        },
    )
    .await;
    let IpcResponse::Audit(entries) = resp else {
        panic!("audit query: {resp:?}");
    };
    // …exactly who ran what:
    let alice_run = entries
        .iter()
        .find(|e| e.action == "job.run" && e.username == "alice" && e.outcome == "ok")
        .expect("alice's run must be audited");
    assert_eq!(alice_run.target, job_id.to_display_string());
    assert!(
        !alice_run.request_sig.is_empty(),
        "provenance travels along"
    );
    // …which secrets her job read:
    assert!(
        entries.iter().any(|e| e.action == "secret.resolve"
            && e.target == "api-token"
            && e.username == "alice"),
        "secret access must be attributed to alice"
    );
    // …and who was turned away:
    assert!(
        entries
            .iter()
            .any(|e| e.username == "vera" && e.outcome.starts_with("denied")),
        "denials are audited"
    );

    // Filtering by user works.
    let resp = ipc_as(
        &a,
        &carol,
        IpcRequest::AuditQuery {
            user: Some("alice".into()),
            action: None,
            since_micros: None,
            limit: 100,
        },
    )
    .await;
    let IpcResponse::Audit(entries) = resp else {
        panic!("filtered audit query: {resp:?}");
    };
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| e.username == "alice"));

    // The auditor runs nothing…
    match run_to_end(&a, &carol, run_req("echo auditors-dont-run", None, vec![])).await {
        RunOutcome::Denied(_) => {}
        other => panic!("auditor must not run: {other:?}"),
    }
    // …and the viewer cannot read the audit log or the secrets list.
    let resp = ipc_as(
        &a,
        &vera,
        IpcRequest::AuditQuery {
            user: None,
            action: None,
            since_micros: None,
            limit: 10,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Denied { .. }), "got {resp:?}");
    let resp = ipc_as(&a, &vera, IpcRequest::Secrets).await;
    assert!(matches!(resp, IpcResponse::Denied { .. }), "got {resp:?}");

    a.node.shutdown();
}

#[tokio::test]
async fn replayed_envelopes_are_rejected() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let admin = founder_user_key(&a);
    let envelope = admin.sign_request(&IpcRequest::WhoAmI).unwrap();
    let first = a.node.handle_signed(envelope.clone()).await;
    assert!(matches!(first, IpcResponse::WhoAmI(_)), "got {first:?}");
    let replay = a.node.handle_signed(envelope).await;
    match replay {
        IpcResponse::Error { message } => {
            assert!(message.contains("replay"), "message: {message}");
        }
        other => panic!("replay must be rejected: {other:?}"),
    }
    a.node.shutdown();
}
