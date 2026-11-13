//! Protocol 1.10: a mesh with more history than one frame can still admit
//! a node.
//!
//! The failure this fixes was not subtle and not recoverable. A joiner was
//! handed the entire op-log in a single `JoinAcceptV2` / `PairCompleteV2`
//! frame; frames are capped at `MAX_FRAME_LEN`; op-logs are never
//! compacted. So every mesh had a size past which it could gossip, run
//! jobs and pass every other test — and never admit another member, with
//! the operator seeing `frame too large: 4493850 bytes` and no way to
//! retry into success.
//!
//! These tests hold the log *above* that line deliberately. A version of
//! them that only checked "more than one batch" would pass against the old
//! code the moment `SYNC_BATCH` was the binding constraint, which is
//! exactly the reassurance that was missing.

use std::time::Duration;

use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::membership::{self, InviteParams};
use mesh_core::pairing::{self, JoinerUx};
use mesh_testkit::{TestNode, founder_user_key, inflate_oplog, join_new_node, spawn_founder};
use tokio::sync::{mpsc, oneshot};

/// Comfortably past the single-frame ceiling, so the old path could not
/// have served this mesh at all.
const OVERSIZED: usize = mesh_proto::MAX_FRAME_LEN as usize + 512 * 1024;

/// Every op the founder holds, as the joiner must end up holding them.
fn op_count(node: &mesh_core::daemon::Node) -> usize {
    node.engine().all_ops().expect("read ops").len()
}

#[tokio::test]
async fn token_join_bootstraps_a_mesh_larger_than_one_frame() {
    let founder = spawn_founder("alpha", "bigmesh").await;
    let bulk = inflate_oplog(&founder.node, OVERSIZED);
    assert!(bulk > 0);

    // The premise: this snapshot genuinely does not fit in one frame.
    let whole = founder.node.engine().all_ops().expect("ops");
    let encoded = postcard::to_stdvec(&whole).expect("encode").len();
    assert!(
        encoded > mesh_proto::MAX_FRAME_LEN as usize,
        "test is only meaningful above the frame ceiling ({encoded} bytes)"
    );

    let joiner = join_new_node("beta", &founder, vec![]).await;

    // Converged, not merely admitted: every op the founder authored to
    // admit it reached the joiner, which is what the instalments have to
    // add up to. Compared by locator rather than by count, because the
    // joiner authors its own boot-time `MemberUpdate` on the way up and a
    // count would be off by exactly that.
    let theirs: std::collections::BTreeSet<(mesh_proto::NodeId, u64)> = founder
        .node
        .engine()
        .all_ops()
        .expect("founder ops")
        .iter()
        .map(|op| (op.author, op.seq))
        .collect();
    let mine: std::collections::BTreeSet<(mesh_proto::NodeId, u64)> = joiner
        .node
        .engine()
        .all_ops()
        .expect("joiner ops")
        .iter()
        .map(|op| (op.author, op.seq))
        .collect();
    assert!(
        theirs.is_subset(&mine),
        "joiner is missing {} op(s) of the mesh's history",
        theirs.difference(&mine).count()
    );
    assert!(op_count(&joiner.node) >= theirs.len());
    let me = joiner.node.node_id();
    assert!(joiner.node.engine().read(|s| s.is_active_member(&me)));
    assert_eq!(
        joiner.node.engine().read(|s| s.audit.len()),
        founder.node.engine().read(|s| s.audit.len())
    );

    joiner.node.shutdown();
    founder.node.shutdown();
}

#[tokio::test]
async fn pairing_bootstraps_a_mesh_larger_than_one_frame() {
    let founder = spawn_founder("alpha", "bigpair").await;
    inflate_oplog(&founder.node, OVERSIZED);

    let dir = tempfile::tempdir().expect("tempdir");
    let config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: Some(dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".to_string()),
            node_name: Some("beta".to_string()),
            mesh_name: Some("ignored-will-pair".to_string()),
            tags: None,
            advertise: None,
        },
    )
    .expect("joiner config");

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let (jdec_tx, jdec_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel();
    let joiner = tokio::spawn({
        let config = config.clone();
        async move {
            pairing::pair_join(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(60)),
            )
            .await
        }
    });
    let addr = listen_rx.await.expect("joiner reports its endpoint");

    let token = membership::create_invite(
        founder.node.engine(),
        founder_user_key(&founder),
        InviteParams {
            ttl_secs: 600,
            uses: 1,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .expect("mint invite");

    let (ev_tx, mut ev_rx) = mpsc::channel(4);
    let (adec_tx, adec_rx) = oneshot::channel();
    let approver = tokio::spawn({
        let transport = founder.node.transport().clone();
        let engine = founder.node.engine().clone();
        async move { pairing::pair_approve(&transport, &engine, addr, token, ev_tx, adec_rx).await }
    });

    tokio::time::timeout(Duration::from_secs(20), code_rx.recv())
        .await
        .expect("joiner code")
        .expect("joiner prompt");
    tokio::time::timeout(Duration::from_secs(20), ev_rx.recv())
        .await
        .expect("approver code")
        .expect("approver event");
    let _ = adec_tx.send(true);
    let _ = jdec_tx.send(true);

    let outcome = approver
        .await
        .expect("approver task")
        .expect("approver side succeeds");
    let report = joiner
        .await
        .expect("joiner task")
        .expect("joiner side succeeds");
    assert_eq!(report.mesh_name, "bigpair");

    // The paired node persisted the whole snapshot, not the first
    // instalment: it can see the founder, itself, and all the history.
    let engine = mesh_core::state::StateEngine::open(
        &config.data_dir,
        mesh_core::identity::Identity::load_or_generate(&config.data_dir)
            .expect("identity")
            .node,
        mesh_core::genesis::Genesis::load(&config.data_dir)
            .expect("load genesis")
            .expect("genesis persisted"),
    )
    .expect("open paired state");
    assert_eq!(
        engine.all_ops().expect("ops").len(),
        op_count(&founder.node)
    );
    assert!(engine.read(|s| s.is_active_member(&outcome.node_id)));

    drop(dir);
    founder.node.shutdown();
}

/// The continuation leg serves op history, so it is authorized as history:
/// only to a node the mesh has already admitted. A dialer that opens a
/// join-purpose connection and skips straight to `JoinFetch` never
/// presented an invite and gets nothing.
#[tokio::test]
async fn join_fetch_without_an_invite_is_refused() {
    let founder = spawn_founder("alpha", "guarded").await;
    inflate_oplog(&founder.node, 256 * 1024);

    let dir = tempfile::tempdir().expect("tempdir");
    let stranger = mesh_core::identity::Identity::load_or_generate(dir.path()).expect("identity");
    let (transport, _events) = mesh_core::transport::Transport::bind(
        "127.0.0.1:0".parse().expect("bind addr"),
        stranger.node.clone(),
        mesh_core::transport::LocalInfo {
            node_id: stranger.node.node_id(),
            node_name: "stranger".into(),
            // Knowing which mesh to dial is not a secret — it rides every
            // invite, including a spent one. Presenting a *token* is the
            // part that admits you, and this dialer never does.
            mesh_id: Some(founder.node.engine().genesis().body.mesh_id),
            mesh_name: None,
        },
        std::sync::Arc::new(|_, _| Ok(())),
        Default::default(),
    )
    .await
    .expect("bind stranger");

    let (conn, _ack) = transport
        .connect_for_join(founder.addr_string().parse().expect("addr"))
        .await
        .expect("join-purpose dial");
    let resp = transport
        .request_one(&conn, &mesh_proto::Request::JoinFetch { vv: Vec::new() })
        .await
        .expect("request completes");
    match resp {
        mesh_proto::Response::Error { message } => {
            assert!(message.contains("not a member"), "{message}");
        }
        other => panic!("history served to a non-member: {other:?}"),
    }

    transport.close();
    founder.node.shutdown();
}

/// A member that is merely *behind* is not a special case — a chunk is a
/// sync batch — but the bootstrap loop must still terminate when the
/// answer is "nothing".
#[tokio::test]
async fn a_caught_up_peer_gets_a_terminal_empty_chunk() {
    let founder: TestNode = spawn_founder("alpha", "empty").await;
    let vv = founder.node.engine().version_vector();
    let chunk =
        membership::snapshot_chunk(founder.node.engine(), &vv, false).expect("snapshot chunk");
    assert!(chunk.ops.is_empty());
    assert!(!chunk.more, "an empty chunk must be terminal");
    assert!(
        chunk.genesis_json.is_empty(),
        "genesis rides chunk one only"
    );

    let first =
        membership::snapshot_chunk(founder.node.engine(), &Vec::new(), true).expect("first chunk");
    assert!(!first.genesis_json.is_empty());

    founder.node.shutdown();
}

/// A joiner too old to receive instalments, against a mesh too large to
/// send one frame, is the one case chunking cannot rescue. What it can do
/// is fail in a sentence the operator can act on rather than a codec error
/// from three layers down.
#[tokio::test]
async fn a_pre_1_10_joiner_is_told_what_to_upgrade() {
    let founder = spawn_founder("alpha", "toobig").await;
    inflate_oplog(&founder.node, OVERSIZED);

    let token = membership::create_invite(
        founder.node.engine(),
        founder_user_key(&founder),
        InviteParams {
            ttl_secs: 600,
            uses: 3,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .expect("mint invite");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = mesh_testkit::test_config_for(dir.path(), "old", "ignored-will-join", vec![]);
    config.proto_minor_override = Some(mesh_proto::BOOTSTRAP_MINOR - 1);
    let err = membership::join_mesh(&config, &token.encode(), &[])
        .await
        .expect_err("the single-frame snapshot cannot carry this mesh");
    let msg = err.to_string();
    assert!(
        msg.contains("instalments") && msg.contains("Upgrade the joining node"),
        "the refusal must name the cause and the fix: {msg}"
    );

    founder.node.shutdown();
}
