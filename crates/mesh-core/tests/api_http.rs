//! **The HTTP API** (E1-P6): the same signed request, over a second
//! transport.
//!
//! These tests speak raw HTTP/1.1 over a real TCP socket to a real daemon,
//! because the whole point of the phase is that something which is not the
//! CLI can drive the mesh — and a test that called the handler directly
//! would prove nothing about that.
//!
//! The property being defended is a negative one, and it is the reason an
//! HTTP surface is safe to add to a governance system at all: **the API
//! decides nothing.** It reads an envelope off a socket and hands it to
//! `handle_signed`, the same function the Unix socket calls. So every gate
//! — RBAC, expiry, change control, audit — applies here *by construction*,
//! and the tests below check exactly that rather than re-testing the gates.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, Role, ServiceAccount, UserRecord};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, spawn_founder_with};

struct Http {
    status: u16,
    request_id: String,
    body: serde_json::Value,
}

/// One request, one connection — the API is `Connection: close`.
async fn send(addr: &str, method: &str, path: &str, body: &[u8]) -> Http {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    s.flush().await.unwrap();

    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete response");
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    let request_id = head
        .lines()
        .find_map(|l| l.strip_prefix("X-Meshware-Request-Id: "))
        .unwrap_or_default()
        .trim()
        .to_string();
    Http {
        status,
        request_id,
        body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    }
}

/// A signed envelope, bound to the node it is sent to — what a request over
/// HTTP has to be (SEC-12).
fn envelope_for(node: &TestNode, who: &Keypair, req: &IpcRequest) -> Vec<u8> {
    let audience = mesh_proto::Audience {
        mesh: node.node.engine().genesis().body.mesh_id,
        node: node.node.node_id(),
    };
    postcard::to_stdvec(&who.sign_request_for(req, &audience).expect("sign")).expect("encode")
}

async fn node_with_api() -> (TestNode, String) {
    let node = spawn_founder_with("alpha", "hyacinth", vec![], |c| {
        // Port 0: the kernel picks, so tests never race for one.
        c.api_addr = Some("127.0.0.1:0".parse().expect("addr"));
    })
    .await;
    let addr = mesh_testkit::wait_until(Duration::from_secs(10), || node.node.api_addr().is_some())
        .await
        .then(|| node.node.api_addr().expect("bound").to_string())
        .expect("the api listener should bind");
    (node, addr)
}

/// The unauthenticated door: what am I talking to? It exists so a client
/// fails with a sentence instead of a parse error, and it discloses nothing
/// a port scan does not.
#[tokio::test]
async fn version_answers_without_a_signature() {
    let (node, addr) = node_with_api().await;
    let r = send(&addr, "GET", "/api/v1/version", b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["api"], "v1");
    assert!(r.body["meshware"].is_string());
    assert!(r.body["protocol_minor"].is_number());
    assert_eq!(r.request_id.len(), 16, "every response is correlatable");
    node.node.shutdown();
}

/// The API is a transport: a signed request over HTTP does what the same
/// signed request does over the socket.
#[tokio::test]
async fn a_signed_request_over_http_does_the_same_thing() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();

    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &admin, &IpcRequest::Nodes),
    )
    .await;
    assert_eq!(r.status, 200);
    let nodes = &r.body["response"]["Nodes"];
    assert!(nodes.is_array(), "expected a node list, got {}", r.body);
    assert_eq!(nodes.as_array().unwrap().len(), 1);
    node.node.shutdown();
}

/// **The load-bearing negative.** A stranger's key is refused over HTTP for
/// exactly the reason it is refused over the socket — the API did not have
/// to remember to check, because it does not check anything.
#[tokio::test]
async fn a_stranger_is_refused_over_http_too() {
    let (node, addr) = node_with_api().await;
    let stranger = Keypair::generate().unwrap();

    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &stranger, &IpcRequest::Nodes),
    )
    .await;
    assert_eq!(r.status, 403, "a stranger must get 403, got {}", r.body);
    node.node.shutdown();
}

/// **The doors open to anybody are limited per address** (SEC-19).
///
/// Asking to enrol writes replicated state and an audit row, and a key
/// costs nothing to make, so a limit per key was no limit. From one
/// address, the eleventh request from eleven fresh keys is refused with a
/// 429; this machine's own socket has a budget of its own. And a sign-in
/// poll has to name a sign-in this node started.
#[tokio::test]
async fn anonymous_requests_are_limited_per_address() {
    let (node, addr) = node_with_api().await;
    let enrol = |who: &Keypair| {
        let now = mesh_proto::now_micros();
        let mut request = mesh_proto::EnrolmentRequest {
            user_id: who.user_id(),
            suggested_username: "visitor".into(),
            from_node: node.node.node_id(),
            nonce: [7u8; 16],
            created_at_micros: now,
            expires_at_micros: now + mesh_proto::enrol::DEFAULT_TTL_SECS * 1_000_000,
            signature: Vec::new(),
        };
        request.signature = who.sign(&request.signing_bytes()).to_vec();
        IpcRequest::EnrolSubmit {
            request: Box::new(request),
        }
    };
    let mut statuses = Vec::new();
    for _ in 0..11 {
        let who = Keypair::generate().unwrap();
        let r = send(
            &addr,
            "POST",
            "/api/v1/ipc",
            &envelope_for(&node, &who, &enrol(&who)),
        )
        .await;
        statuses.push(r.status);
        if r.status == 429 {
            let message = r.body["response"]["Error"]["message"]
                .as_str()
                .unwrap_or("");
            assert!(message.contains("enrolment"), "{}", r.body);
        }
    }
    assert_eq!(statuses[..10], [200; 10], "{statuses:?}");
    assert_eq!(statuses[10], 429, "{statuses:?}");
    assert_eq!(node.node.engine().read(|s| s.enrolments.len()), 10);

    // The local socket is another source.
    let local = Keypair::generate().unwrap();
    let resp = ipc_as(&node, &local, enrol(&local)).await;
    assert!(
        matches!(resp, mesh_proto::IpcResponse::Ok),
        "the node's own socket shares no budget with a remote address: {resp:?}"
    );

    // A poll for a sign-in nobody started here never reaches a provider.
    let stranger = Keypair::generate().unwrap();
    let poll = IpcRequest::OidcLoginPoll {
        provider: "entra".into(),
        device_code: "made-up".into(),
        interval: 5,
    };
    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &stranger, &poll),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.body);
    let message = r.body["response"]["Error"]["message"]
        .as_str()
        .unwrap_or("");
    assert!(message.contains("no sign-in with that code"), "{}", r.body);
    node.node.shutdown();
}

/// **The one thing the API does decide** (SEC-3). `Push` and `Backup` name
/// a path on the node's own filesystem, and a caller over HTTP is not on
/// that machine — so even the founder is refused, and the node's identity
/// cannot be read into a bundle from across the network.
#[tokio::test]
async fn requests_that_name_a_node_path_are_local_only() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();
    let identity = node.config().data_dir.join("identity");
    let scratch = tempfile::tempdir().unwrap();

    for req in [
        IpcRequest::Push {
            path: identity.to_string_lossy().into(),
        },
        IpcRequest::Push {
            path: scratch.path().to_string_lossy().into(),
        },
        IpcRequest::Backup {
            path: scratch.path().join("b").to_string_lossy().into(),
        },
    ] {
        let r = send(
            &addr,
            "POST",
            "/api/v1/ipc",
            &envelope_for(&node, &admin, &req),
        )
        .await;
        assert_eq!(r.status, 400, "{req:?} over HTTP: {}", r.body);
        let message = r.body["response"]["Error"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("local socket only"), "{message}");
    }
    // The same request over the socket is still served.
    let resp = ipc_as(
        &node,
        &admin,
        IpcRequest::Push {
            path: scratch.path().to_string_lossy().into(),
        },
    )
    .await;
    assert!(
        matches!(resp, mesh_proto::IpcResponse::Pushed { .. }),
        "{resp:?}"
    );
    node.node.shutdown();
}

/// An unsigned or corrupt body is a 400 with a code a client can branch on,
/// never a panic and never a 500.
#[tokio::test]
async fn a_body_that_is_not_an_envelope_is_a_structured_400() {
    let (node, addr) = node_with_api().await;

    for body in [&b""[..], b"{\"hello\":1}", b"\xff\xff\xff\xff"] {
        let r = send(&addr, "POST", "/api/v1/ipc", body).await;
        assert_eq!(r.status, 400, "body {body:?} should be a 400");
        assert_eq!(r.body["error"]["code"], "malformed_envelope");
        assert!(!r.body["request_id"].as_str().unwrap_or_default().is_empty());
    }
    node.node.shutdown();
}

#[tokio::test]
async fn unknown_paths_and_methods_are_refused_in_one_shape() {
    let (node, addr) = node_with_api().await;

    let r = send(&addr, "GET", "/", b"").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.body["error"]["code"], "not_found");

    // The observability endpoint is a *different listener*, deliberately —
    // it is unauthenticated, and sharing a port would mean exposing the API
    // silently exposed the metrics.
    let r = send(&addr, "GET", "/metrics", b"").await;
    assert_eq!(r.status, 404, "metrics must not be on the api listener");

    let r = send(&addr, "DELETE", "/api/v1/ipc", b"").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.body["error"]["code"], "method_not_allowed");
    node.node.shutdown();
}

/// A query string must not turn a known path into a 404.
#[tokio::test]
async fn a_query_string_is_ignored_rather_than_breaking_the_route() {
    let (node, addr) = node_with_api().await;
    let r = send(&addr, "GET", "/api/v1/version?pretty=1", b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["api"], "v1");
    node.node.shutdown();
}

/// **The intended credential is a machine principal** (E1-P4), and this is
/// the phase-ordering paying off: an API without one would have forced a
/// human's long-lived key file onto a CI runner.
#[tokio::test]
async fn a_service_account_is_the_credential_an_api_client_uses() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();

    // Mint one the way `meshnode serviceaccounts create` does.
    let ci = Keypair::generate().unwrap();
    let mut record = UserRecord {
        user_id: ci.user_id(),
        username: "ci-deploy".into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::UserAdd { record }).await;

    let mut account = ServiceAccount {
        user_id: ci.user_id(),
        name: "ci-deploy".into(),
        description: "api client".into(),
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        expires_at_micros: None,
        signature: Vec::new(),
    };
    account.signature = admin.sign(&account.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::ServiceAccountAdd { account }).await;

    let mut grant = mesh_proto::Grant {
        grant_id: mesh_proto::GrantId::generate(),
        user_id: ci.user_id(),
        role: Role::Developer,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::GrantAdd { grant }).await;

    // …and it works over HTTP.
    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &ci, &IpcRequest::Nodes),
    )
    .await;
    assert_eq!(
        r.status, 200,
        "a granted service account should be able to read over the API: {}",
        r.body
    );
    node.node.shutdown();
}

/// **Expiry reaches the API too**, because the API does not evaluate it —
/// `roles_at` does, on the same dispatch. A test that had to assert this
/// separately would be evidence the design was wrong.
#[tokio::test]
async fn an_expired_service_account_is_refused_over_http() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();
    let ci = Keypair::generate().unwrap();
    let past = mesh_proto::now_micros().saturating_sub(60 * 1_000_000);

    let mut record = UserRecord {
        user_id: ci.user_id(),
        username: "ci-stale".into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::UserAdd { record }).await;

    let mut account = ServiceAccount {
        user_id: ci.user_id(),
        name: "ci-stale".into(),
        description: String::new(),
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        expires_at_micros: Some(past),
        signature: Vec::new(),
    };
    account.signature = admin.sign(&account.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::ServiceAccountAdd { account }).await;

    let mut grant = mesh_proto::Grant {
        grant_id: mesh_proto::GrantId::generate(),
        user_id: ci.user_id(),
        role: Role::Developer,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    ipc_as(&node, &admin, IpcRequest::GrantAdd { grant }).await;

    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &ci, &IpcRequest::Nodes),
    )
    .await;
    assert_eq!(
        r.status, 403,
        "an expired principal must be refused over HTTP: {}",
        r.body
    );
    node.node.shutdown();
}

/// A replayed envelope is refused, because `SignedIpc` already carries a
/// nonce and the daemon already keeps a replay guard. The API inherits it
/// along with everything else.
#[tokio::test]
async fn a_replayed_envelope_is_refused() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();
    let body = envelope_for(&node, &admin, &IpcRequest::Nodes);

    let first = send(&addr, "POST", "/api/v1/ipc", &body).await;
    assert_eq!(first.status, 200);

    // Byte-identical, so the nonce has been seen.
    let second = send(&addr, "POST", "/api/v1/ipc", &body).await;
    assert_ne!(
        second.status, 200,
        "a byte-identical envelope must not be accepted twice: {}",
        second.body
    );
    node.node.shutdown();
}

/// SEC-12: an envelope names the node it is for. Unbound (`mw-ipc-v1`),
/// or bound to another node, it is refused over HTTP — so a request read
/// off the wire at one node is no good at the next.
#[tokio::test]
async fn a_request_over_http_must_name_this_node() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();
    let unbound = postcard::to_stdvec(&admin.sign_request(&IpcRequest::Nodes).unwrap()).unwrap();
    let r = send(&addr, "POST", "/api/v1/ipc", &unbound).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body.to_string().contains("signed for this node"),
        "{}",
        r.body
    );

    // A second node of the same mesh, also serving the API.
    let beta = mesh_testkit::join_new_node_with("beta", &node, vec![], |c| {
        c.api_addr = Some("127.0.0.1:0".parse().expect("addr"));
    })
    .await;
    let beta_addr =
        mesh_testkit::wait_until(Duration::from_secs(10), || beta.node.api_addr().is_some())
            .await
            .then(|| beta.node.api_addr().unwrap().to_string())
            .unwrap();
    let for_alpha = envelope_for(&node, &admin, &IpcRequest::Nodes);
    let r = send(&beta_addr, "POST", "/api/v1/ipc", &for_alpha).await;
    assert_eq!(
        r.status, 400,
        "an envelope for alpha was served by beta: {}",
        r.body
    );
    let r = send(&addr, "POST", "/api/v1/ipc", &for_alpha).await;
    assert_eq!(r.status, 200, "{}", r.body);

    // Work submitted over HTTP runs on another node, which accepts an
    // envelope bound to the node that took the request.
    let run = IpcRequest::Run {
        command: "echo over-http".into(),
        on: Some("name=beta".into()),
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let beta_id = beta.node.node_id();
    assert!(
        mesh_testkit::wait_until(Duration::from_secs(20), || {
            node.node.engine().read(|s| s.is_active_member(&beta_id))
                && matches!(
                    node.node.gossip().liveness_of(&beta_id),
                    mesh_proto::Liveness::Alive
                )
        })
        .await
    );
    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &admin, &run),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let job_id = r.body["response"]["JobStarted"]["job_id"].clone();
    assert!(!job_id.is_null(), "{}", r.body);
    assert!(
        mesh_testkit::wait_until(Duration::from_secs(20), || {
            beta.node.engine().read(|s| {
                s.jobs.values().any(|j| {
                    j.cmd_preview.contains("over-http")
                        && j.state == mesh_proto::JobState::Succeeded
                })
            })
        })
        .await,
        "the job never ran on beta"
    );
    beta.node.shutdown();
    node.node.shutdown();
}

/// SEC-12: a streamed request answers with its first frame and does not
/// hold the listener. It used to block forever past sixteen frames while
/// holding one of sixty-four permits, so a few dozen ordinary runs left the
/// API answering 503 until restart.
#[tokio::test]
async fn streamed_requests_do_not_wedge_the_api() {
    let (node, addr) = node_with_api().await;
    let admin = founder_user_key(&node).clone();
    // Well over sixteen frames of output each — paced, so each line is its
    // own frame — and more calls than the listener has permits.
    let run = IpcRequest::Run {
        command: "for i in $(seq 1 30); do echo line-$i; sleep 0.05; done".into(),
        on: None,
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    for i in 0..70 {
        let r = send(
            &addr,
            "POST",
            "/api/v1/ipc",
            &envelope_for(&node, &admin, &run),
        )
        .await;
        assert_eq!(r.status, 200, "run {i}: {}", r.body);
        assert!(
            !r.body["response"]["JobStarted"].is_null(),
            "run {i}: {}",
            r.body
        );
    }
    let r = send(
        &addr,
        "POST",
        "/api/v1/ipc",
        &envelope_for(&node, &admin, &IpcRequest::Nodes),
    )
    .await;
    assert_eq!(r.status, 200, "the api is wedged: {}", r.body);
    node.node.shutdown();
}

/// The API is off unless somebody asked for it. A management surface that
/// appears because a fleet upgraded is a management surface nobody decided
/// to run.
#[tokio::test]
async fn there_is_no_listener_unless_it_is_configured() {
    let node = mesh_testkit::spawn_founder("alpha", "hyacinth").await;
    assert!(
        node.node.api_addr().is_none(),
        "the api must be off by default"
    );
    node.node.shutdown();
}
