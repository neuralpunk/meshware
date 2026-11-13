//! **Director talks to the node the way `meshnode` does** (2026-09).
//!
//! Director used to embed the node it showed and call it in process, where
//! a streamed answer ended because the node dropped its end of a channel.
//! It is now a client of this machine's node over the socket, through
//! `ipc::Client`, and over a socket nothing is dropped: the daemon finishes
//! a stream and waits for the next request on the same connection. These
//! pin the property Director leans on hardest — that a stream still *ends*,
//! with the client never having to know which frame is the last one.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use mesh_core::ipc::Client;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder};

/// Serve `node`'s socket the way the daemon does, and wait for it to answer.
async fn serve(node: &TestNode) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run").join("meshnode.sock");
    let (served, at) = (node.node.clone(), socket.clone());
    tokio::spawn(async move {
        let _ = mesh_core::ipc::serve(at, served).await;
    });
    for _ in 0..100 {
        if mesh_core::ipc::connect(&socket).await.is_ok() {
            return (socket, dir);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the socket never answered");
}

fn run(node: &TestNode, command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.to_string(),
        on: Some(format!("name={}", node.config().node_name)),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

#[tokio::test]
async fn a_single_answer_comes_back_whole() {
    let founder = spawn_founder("NIM", "ipc-client-one").await;
    let (socket, _dir) = serve(&founder).await;
    let client = Client::new(socket, founder_user_key(&founder).clone());

    let resp = client.request(IpcRequest::Status).await.unwrap();
    assert!(matches!(resp, IpcResponse::Status(_)), "{resp:?}");
}

#[tokio::test]
async fn a_stream_ends_when_the_node_has_nothing_more_to_say() {
    let founder = spawn_founder("NIM", "ipc-client-stream").await;
    let (socket, _dir) = serve(&founder).await;
    let client = Client::new(socket, founder_user_key(&founder).clone());

    // One frame, then the end. Before the half-close this hung: the daemon
    // had answered and was waiting, politely, for a second request.
    let mut rx = client.stream(IpcRequest::Status).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("an answer");
    assert!(matches!(first, Some(IpcResponse::Status(_))), "{first:?}");
    let end = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the stream must end, not wait for ever");
    assert!(end.is_none(), "one frame, then the end: {end:?}");

    // Many frames, then the end — the client is never told that `JobDone`
    // is the last word, and does not need to be.
    let mut rx = client
        .stream(run(&founder, "echo from-the-socket"))
        .await
        .unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .expect("a run's stream must end once the run has")
    {
        frames.push(frame);
    }
    assert!(
        frames
            .iter()
            .any(|f| matches!(f, IpcResponse::JobDone { .. })),
        "{frames:?}"
    );
    let stdout: String = frames
        .iter()
        .filter_map(|f| match f {
            IpcResponse::LogChunk { stream: 1, data } => Some(String::from_utf8_lossy(data)),
            _ => None,
        })
        .collect();
    assert!(stdout.contains("from-the-socket"), "{stdout:?}");
}

/// A service's whole live version — `env` included, which `ServiceShow`
/// leaves out — for a client about to sign the next version from it.
///
/// Director used to read this straight out of the node it embedded, where
/// no permission applied at all. Asked over the socket it is held to what
/// publishing that next version would take: the founder gets it, a key the
/// mesh does not know does not.
#[tokio::test]
async fn a_service_comes_back_whole_to_whoever_may_publish_its_next_version() {
    let founder = spawn_founder("NIM", "ipc-client-service").await;
    let admin = founder_user_key(&founder).clone();
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("run.sh"), "echo hi\n").unwrap();
    let engine = founder.node.engine();
    let content =
        mesh_core::content::ContentStore::open(founder.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).unwrap();
    mesh_core::services::create_service_as(
        engine,
        &admin,
        "greet".into(),
        mesh_proto::Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![("GREETING".into(), "hello".into())],
        String::new(),
        vec![],
    )
    .unwrap();
    let (socket, _dir) = serve(&founder).await;

    let resp = Client::new(socket.clone(), admin)
        .request(IpcRequest::ServiceCurrent {
            name: "greet".into(),
        })
        .await
        .unwrap();
    let IpcResponse::ServiceCurrent(version) = resp else {
        panic!("{resp:?}");
    };
    assert_eq!(version.env, vec![("GREETING".into(), "hello".into())]);

    let stranger = mesh_core::identity::Keypair::generate().unwrap();
    let resp = Client::new(socket, stranger)
        .request(IpcRequest::ServiceCurrent {
            name: "greet".into(),
        })
        .await
        .unwrap();
    assert!(matches!(resp, IpcResponse::Denied { .. }), "{resp:?}");
}
