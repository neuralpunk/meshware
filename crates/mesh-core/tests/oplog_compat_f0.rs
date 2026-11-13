//! F0: the forward-compatible op-log (M10–M13 plan §3).
//!
//! The claim under test is narrow and load-bearing: **a node can verify,
//! store and forward an op whose body it cannot decode.** Everything M10,
//! M11 and M12 want to put in the op-log rests on it, and the failure mode
//! it replaces is not "a feature is missing" but "mixed-version meshes stop
//! converging, permanently" (§3.1).
//!
//! Four properties, in the order they matter:
//!
//! 1. An unknown body is authenticated, applied as a no-op, persisted and
//!    **relayed to a third node** — carriage is what convergence needs;
//!    interpretation is local.
//! 2. A pre-1.7 peer is served a *truncated* view rather than a broken one,
//!    and its version vector is truncated to match so it stops asking.
//! 3. That peer stays healthy, converges on everything it can decode, and
//!    says exactly what it is missing (`nodes`, `doctor`).
//! 4. A pre-F0 data dir and a pre-F0 backup both carry forward.

use std::time::Duration;

use mesh_core::state::StateEngine;
use mesh_proto::{IpcRequest, IpcResponse, Liveness, MemberInfo, OpBody, OpEnvelope};
use mesh_testkit::ops::{future_body, now_hlc, seal_raw};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, join_new_node_with, spawn_founder,
    spawn_founder_with, wait_until,
};

/// The variant index M10 P1's first workflow op will take. Using the exact
/// number keeps this test honest: it is not a wildly-out-of-range value the
/// decoder might reject for a different reason.
const FIRST_FUTURE_INDEX: u32 = mesh_proto::V1_OPBODY_VARIANTS;

/// Author an op on `node` whose body no build in this workspace can decode,
/// as though a 1.8 node had written it. Signed with the node's real key, so
/// it is indistinguishable from a legitimate op except for the one thing
/// that matters — nobody here has the variant.
fn inject_future_op(node: &TestNode, index: u32, payload: &[u8]) -> OpEnvelope {
    let key = node.node.node_key_for_test();
    let engine = node.node.engine();
    let seq = engine.read(|s| s.applied.get(&key.node_id()).copied().unwrap_or(0)) + 1;
    let op = seal_raw(key, seq, now_hlc(), future_body(index, payload));
    let applied = engine.ingest(vec![op.clone()]).expect("ingest future op");
    assert_eq!(
        applied, 1,
        "an authentic future op must advance the watermark"
    );
    op
}

fn has_op(node: &TestNode, op: &OpEnvelope) -> bool {
    node.node
        .engine()
        .all_ops()
        .expect("all_ops")
        .iter()
        .any(|o| o.author == op.author && o.seq == op.seq && o.body == op.body)
}

// ---------------------------------------------------------------------------
// 1. Carry what you cannot read
// ---------------------------------------------------------------------------

/// The F0.1 exit demo, at mesh scale: a node ingests an op with a body
/// index it has no code for, advances its watermark, keeps converging —
/// and the op reaches a *third* node that also cannot read it. Relay of an
/// uninterpretable op is the whole mechanism; without it the third node
/// would silently diverge from the second.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_op_is_carried_verified_and_relayed() {
    let a = spawn_founder("a", "f0-carry").await;
    let b = join_new_node("b", &a, vec![]).await;
    let c = join_new_node("c", &a, vec![]).await;
    assert!(
        wait_until(Duration::from_secs(15), || {
            [&a, &b, &c]
                .iter()
                .all(|n| n.node.engine().read(|s| s.members.len() == 3))
        })
        .await,
        "membership did not converge"
    );

    // b authors something only a future release understands.
    let future = inject_future_op(&b, FIRST_FUTURE_INDEX, b"a workflow definition, one day");
    assert!(
        future.decode_body().is_none(),
        "the fixture must be undecodable, or this test proves nothing"
    );

    // It reaches a and c through ordinary gossip and is stored verbatim.
    assert!(
        wait_until(Duration::from_secs(20), || has_op(&a, &future)
            && has_op(&c, &future))
        .await,
        "an unknown op must still propagate to every member"
    );

    // Every node's watermark reached it, so nothing behind it parks.
    //
    // **At least, not exactly.** `b` keeps authoring while this waits for
    // gossip — an address refresh is enough — so pinning the watermark to
    // `future.seq` asserted "and nothing else happened for twenty seconds",
    // which is not the property and fails on a quick machine. What F0
    // claims is that an undecodable body does not *park* the ops behind it,
    // and that is `>=`.
    for node in [&a, &b, &c] {
        let watermark = node
            .node
            .engine()
            .read(|s| s.applied.get(&future.author).copied().unwrap_or(0));
        assert!(
            watermark >= future.seq,
            "watermark {watermark} did not advance past the unknown op at seq {} on {:?}",
            future.seq,
            node.node.node_id().short()
        );
    }

    // And the mesh keeps working: ops authored *after* the unknown one
    // apply everywhere, which is precisely what a gap would have prevented.
    b.node
        .engine()
        .author_op(OpBody::MemberUpdate {
            node_id: b.node.node_id(),
            name: "b-renamed".into(),
            addrs: vec![b.addr_string()],
            tags: vec![],
            slots: 4,
        })
        .expect("author after the unknown op");
    assert!(
        wait_until(Duration::from_secs(20), || {
            [&a, &c].iter().all(|n| {
                n.node.engine().read(|s| {
                    s.members
                        .get(&b.node.node_id())
                        .is_some_and(|m| m.info.name == "b-renamed")
                })
            })
        })
        .await,
        "sync wedged behind an op nobody could decode — the exact F0 failure"
    );
}

/// A forged op with an unknown body is rejected like any other forgery.
/// Carrying what you cannot read must not become trusting what you cannot
/// read: the signature is checked over the encoded body, so authenticity
/// does not depend on comprehension.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_op_with_a_bad_signature_is_dropped() {
    let a = spawn_founder("a", "f0-forge").await;
    let engine = a.node.engine();
    let key = a.node.node_key_for_test();
    let seq = engine.read(|s| s.applied.get(&key.node_id()).copied().unwrap_or(0)) + 1;

    let mut op = seal_raw(
        key,
        seq,
        now_hlc(),
        future_body(FIRST_FUTURE_INDEX, b"forged"),
    );
    op.signature[0] ^= 0xff;
    assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);

    // Tampering with the body invalidates the signature too — the body is
    // what the signature covers, byte for byte.
    let mut op = seal_raw(
        key,
        seq,
        now_hlc(),
        future_body(FIRST_FUTURE_INDEX, b"honest"),
    );
    op.body.push(0x42);
    assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);
    assert_eq!(
        engine.read(|s| s.applied.get(&key.node_id()).copied().unwrap_or(0)),
        seq - 1,
        "a forged op must not advance the watermark"
    );
}

// ---------------------------------------------------------------------------
// 2 & 3. The V1 downgrade rule
// ---------------------------------------------------------------------------

/// The F0.3 exit demo. One 1.6 node and two 1.7 nodes in one mesh, with a
/// future op kind flowing through it:
///
/// - the 1.6 node converges on everything it *can* decode,
/// - it never receives the op it cannot,
/// - it never spins and never goes unhealthy,
/// - and both 1.7 nodes report exactly what it is missing and why.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_f0_peer_is_stably_behind_not_wedged() {
    // Facts off on the admitting member: since M18 every node publishes a
    // `FactsPublish` at boot, and that op's index is one a 1.6 build has no
    // variant for — so a member that has published facts can no longer
    // admit a pre-1.7 joiner at all. That is a real consequence and it has
    // a test of its own (`facts_m18.rs`); what this test is about is
    // carriage, so the door is held open deliberately.
    let a = spawn_founder_with("a", "f0-mixed", vec![], |c| c.facts.collect = false).await;
    // Off on `b` too, so the *only* op the old node cannot read is the one
    // this test injects — otherwise the watermark stops at b's fact publish
    // and the assertion below would be measuring the wrong cut point.
    let b = join_new_node_with("b", &a, vec![], |c| c.facts.collect = false).await;
    let old = join_new_node_with("old", &a, vec![], |c| {
        c.proto_minor_override = Some(mesh_proto::OPLOG_MINOR - 1);
    })
    .await;
    let old_id = old.node.node_id();
    assert!(
        wait_until(Duration::from_secs(20), || {
            [&a, &b, &old]
                .iter()
                .all(|n| n.node.engine().read(|s| s.members.len() == 3))
        })
        .await,
        "membership did not converge across the version boundary"
    );
    assert_eq!(
        a.node.transport().peer_minor(&old_id),
        Some(mesh_proto::OPLOG_MINOR - 1),
        "the old node must actually negotiate down"
    );

    // b authors an op the old node has no variant for.
    let future = inject_future_op(&b, FIRST_FUTURE_INDEX, b"policy, one day");
    assert!(
        wait_until(Duration::from_secs(20), || has_op(&a, &future)).await,
        "the 1.7 peer must receive it"
    );

    // Give gossip many rounds. The old node never gets that op...
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !has_op(&old, &future),
        "a pre-1.7 node must never be handed a body it cannot decode"
    );
    // ...and its watermark for b stops just before it, exactly as the
    // truncated version vector told it to.
    assert_eq!(
        old.node
            .engine()
            .read(|s| s.applied.get(&future.author).copied().unwrap_or(0)),
        future.seq - 1,
        "the old node should be caught up to just before the op it cannot read"
    );

    // It is not wedged: it is alive, it still has full membership, and
    // ops from *other* authors keep flowing to it.
    assert!(
        matches!(a.node.gossip().liveness_of(&old_id), Liveness::Alive),
        "gossip with a pre-1.7 peer went unhealthy"
    );
    a.node
        .engine()
        .author_op(OpBody::MemberUpdate {
            node_id: a.node.node_id(),
            name: "a-renamed".into(),
            addrs: vec![a.addr_string()],
            tags: vec![],
            slots: 4,
        })
        .expect("author on a");
    assert!(
        wait_until(Duration::from_secs(20), || old.node.engine().read(|s| s
            .members
            .get(&a.node.node_id())
            .is_some_and(|m| m.info.name == "a-renamed")))
        .await,
        "an author with no future ops must keep flowing to the old node"
    );

    // And the mesh says so, in both surfaces an operator would look at.
    let user = founder_user_key(&a);
    let IpcResponse::Nodes(rows) = ipc_as(&a, user, IpcRequest::Nodes).await else {
        panic!("expected Nodes");
    };
    let row = rows
        .iter()
        .find(|r| r.node_id == old_id)
        .expect("old node in the table");
    let stale = row.stale.expect("the old node must be marked stale");
    assert_eq!(stale.minor, mesh_proto::OPLOG_MINOR - 1);
    assert!(stale.ops_withheld >= 1, "it is missing at least the one op");
    assert_eq!(stale.authors_blocked, 1, "only b authored a future op");

    // Nobody else is marked: a 1.7 peer is owed nothing.
    assert!(
        rows.iter()
            .filter(|r| r.node_id != old_id)
            .all(|r| r.stale.is_none()),
        "only the pre-1.7 peer should be marked stale"
    );
}

/// A pre-F0 joiner is admitted normally by a member that has authored no
/// future ops, even when the *mesh* holds some — the cut is per author, and
/// the joiner's own `MemberAdd` comes from the member admitting it.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_f0_node_joins_through_a_member_that_has_no_future_ops() {
    // `[facts] collect = "off"` is what "has no future ops" means for a
    // member since M18 — see the note in the test above.
    let a = spawn_founder_with("a", "f0-join-ok", vec![], |c| c.facts.collect = false).await;
    let b = join_new_node("b", &a, vec![]).await;
    let future = inject_future_op(&b, FIRST_FUTURE_INDEX, b"host record, one day");
    assert!(
        wait_until(Duration::from_secs(20), || has_op(&a, &future)).await,
        "a must carry b's future op before we test the join"
    );

    // a has authored nothing the joiner cannot read, so it can admit it.
    let old = join_new_node_with("old", &a, vec![], |c| {
        c.proto_minor_override = Some(mesh_proto::OPLOG_MINOR - 1);
    })
    .await;
    assert!(
        wait_until(Duration::from_secs(20), || {
            a.node
                .engine()
                .read(|s| s.is_active_member(&old.node.node_id()))
                && old.node.engine().read(|s| s.members.len() == 3)
        })
        .await,
        "a pre-1.7 node must still join through an unaffected member"
    );
    // It simply never sees the op it has no code for.
    assert!(!has_op(&old, &future));
}

/// The op indices M18 and M19 actually allocated.
///
/// Not a formality. `postcard` encodes an enum variant by index, and every
/// op signature ever produced covers those bytes — an index that shifted
/// because somebody inserted a variant instead of appending would
/// invalidate a mesh's entire history silently. §9's rule is "appending,
/// never inserting", and this is what turns that from a convention into
/// something a build can fail on.
///
/// **Renumbered once, on purpose: 46/47 under 1.x, 27/28 since protocol
/// 2.0** (2026-09-20). Thirty retired variants ahead of these were deleted
/// rather than kept as tombstones, which moves every index after the first
/// deletion — the exact thing this test exists to catch. It was allowed
/// because it rode a *major* bump: a 1.x op-log is refused at open with the
/// remedy named (`a_pre_2_0_data_dir_is_refused_rather_than_misread`), so
/// no signature is ever checked against the new numbering. Changing these
/// numbers again without a major bump is the silent corruption above.
#[test]
fn the_new_bodies_took_the_indices_the_plan_reserved() {
    use mesh_proto::{FactCollector, FactSubject, StepAction, UserId};

    let collector = FactCollector {
        name: "gpu".into(),
        run: StepAction::Command("true".into()),
        on: "any".into(),
        interval_secs: 300,
        subject: mesh_proto::FactSubjectKind::Node,
        paused: false,
        timeout_secs: None,
        description: String::new(),
        created_by: UserId([0u8; 32]),
        created_at_micros: 0,
        signature: vec![0u8; 64],
    };
    for (body, expected) in [
        (
            OpBody::FactCollectorPut {
                collector: Box::new(collector),
            },
            27u32,
        ),
        (
            OpBody::FactsPublish {
                subject: FactSubject::Node(mesh_proto::NodeId([1u8; 32])),
                collector: "system".into(),
                doc: b"{}".to_vec(),
                digest: [0u8; 32],
                collected_at_micros: 0,
            },
            mesh_proto::OPBODY_FACTSPUBLISH_INDEX,
        ),
    ] {
        let encoded = postcard::to_stdvec(&body).expect("encode");
        assert_eq!(
            mesh_proto::op_body_index(&encoded),
            Some(expected),
            "{body:?} must keep index {expected}"
        );
        assert!(
            expected >= mesh_proto::V1_OPBODY_VARIANTS,
            "a post-1.6 body must sort above the V1 cut"
        );
    }
}

/// A node with no code for M18's bodies carries them, converges on
/// everything else, and — the part that matters — **matches nothing** on a
/// `fact.*` selector rather than everything (§11.3).
///
/// The first half is F0's guarantee, exercised here with the real indices.
/// The second half is M18's, and it is the only reason adding a selector
/// variant to a signed string was safe.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_without_fact_code_carries_the_ops_and_matches_nothing() {
    let a = spawn_founder("a", "f0-facts").await;
    let b = join_new_node("b", &a, vec![]).await;
    assert!(
        wait_until(Duration::from_secs(20), || {
            [&a, &b]
                .iter()
                .all(|n| n.node.engine().read(|s| s.members.len() == 2))
        })
        .await,
        "membership did not converge"
    );

    // A body at an index past everything this build knows — what a 1.10
    // node's next entity kind will look like to *us*, which is exactly what
    // M18's bodies look like to a 1.8 node.
    let future = inject_future_op(&b, FIRST_FUTURE_INDEX + 20, b"facts, one day");
    assert!(
        wait_until(Duration::from_secs(20), || has_op(&a, &future)).await,
        "an op past this build's variants must still be carried"
    );

    // And the fail-closed half: an undecodable fact document means no fact,
    // and no fact means no match. Never "match everything".
    let user = founder_user_key(&a).clone();
    match ipc_as(
        &a,
        &user,
        IpcRequest::TargetPreview {
            selector: "fact.something_nobody_publishes=yes".into(),
        },
    )
    .await
    {
        IpcResponse::Nodes(rows) => assert!(rows.is_empty(), "{rows:?}"),
        other => panic!("target preview: {other:?}"),
    }
}

/// A pre-F0 joiner asking a member that *has* authored a future op is
/// refused, with the two remedies named — because the `MemberAdd` admitting
/// it would land after that member's cut, so it would come up believing it
/// is not in the mesh. Refusing costs an invite use nothing and leaves no
/// half-member behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_f0_joiner_is_refused_by_a_member_that_is_already_ahead() {
    use mesh_core::membership;

    let a = spawn_founder("a", "f0-join-refuse").await;
    inject_future_op(&a, FIRST_FUTURE_INDEX, b"workflow, one day");

    let token = membership::create_invite(
        a.node.engine(),
        founder_user_key(&a),
        membership::InviteParams {
            ttl_secs: 3600,
            uses: 10,
            tags: vec![],
            bootstrap: vec![a.addr_string()],
        },
    )
    .expect("create invite");
    let invite_id = token.invite_id;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = mesh_testkit::test_config_for(dir.path(), "old", "ignored-will-join", vec![]);
    config.proto_minor_override = Some(mesh_proto::OPLOG_MINOR - 1);
    let err = membership::join_mesh(&config, &token.encode(), &[])
        .await
        .expect_err("a pre-1.7 joiner must be refused, not half-admitted");
    let msg = err.to_string();
    assert!(
        msg.contains("Upgrade the joining node") && msg.contains("join through a member"),
        "the refusal must name both remedies: {msg}"
    );

    // Nothing was spent and nobody was added.
    assert_eq!(
        a.node.engine().read(|s| s.invite_uses_left(&invite_id)),
        Some(10),
        "a refused join must not consume an invite use"
    );
    assert_eq!(
        a.node.engine().read(|s| s.members.len()),
        1,
        "a refused join must not leave a member record"
    );
}

// ---------------------------------------------------------------------------
// 4. Carrying data forward
// ---------------------------------------------------------------------------

/// A data dir written before protocol 2.0 is **refused at open with the
/// remedy named**, not migrated and not misread.
///
/// F0 migrated a pre-envelope log in place, because the op *bodies* were
/// unchanged and only the row encoding moved. 2.0 is the other kind of
/// change: thirty retired `OpBody` variants were deleted rather than kept
/// as tombstones, so every index after the first deletion means something
/// different now — and every op's signature covers those bytes. Replaying
/// a 1.x log under 2.0's numbering would read ops as the wrong kind and
/// then fail their signatures, reporting corruption on data that is not
/// corrupt. Refusing is the only honest answer, and it has to *say* what to
/// do instead.
#[test]
fn a_pre_2_0_data_dir_is_refused_rather_than_misread() {
    use mesh_core::genesis::Genesis;
    use mesh_core::identity::Keypair;
    use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableDefinition};

    const OPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("ops");
    const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

    /// Build a normal log, then put it back into an older on-disk shape:
    /// `stamp` is the format number to leave behind, or `None` to remove
    /// the stamp entirely the way format 1 had it.
    fn make(dir: &std::path::Path, stamp: Option<u64>) -> (Keypair, Genesis, usize) {
        let founder_user = Keypair::generate().expect("user key");
        let founder_node = Keypair::generate().expect("node key");
        let genesis =
            Genesis::create("f0-refuse", &founder_user, founder_node.node_id()).expect("genesis");
        let names = ["alpha", "beta", "gamma"];
        {
            let engine =
                StateEngine::open(dir, founder_node.clone(), genesis.clone()).expect("open");
            for name in names {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: MemberInfo {
                            node_id: founder_node.node_id(),
                            name: name.into(),
                            addrs: vec![],
                            tags: vec![],
                            slots: 1,
                            joined_at_micros: mesh_proto::now_micros(),
                        },
                        invite: None,
                    })
                    .expect("author");
            }
        }
        let db = Database::create(dir.join("state/oplog.redb")).expect("open raw");
        let txn = db.begin_write().expect("write txn");
        {
            let mut meta = txn.open_table(META).expect("meta");
            match stamp {
                Some(v) => {
                    meta.insert("format", v).expect("restamp");
                }
                None => {
                    meta.remove("format").expect("unstamp");
                }
            }
        }
        txn.commit().expect("commit");
        (founder_node, genesis, names.len())
    }

    // Format 2 — a 1.x log, stamped. The message has to name the version
    // gap and a way out, because "cannot be read" alone leaves somebody
    // with a directory they will delete.
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let (node, genesis, ops) = make(dir.path(), Some(2));
        let err = StateEngine::open(dir.path(), node, genesis).expect_err("format 2 is refused");
        let msg = err.to_string();
        assert!(msg.contains("protocol 1.x"), "{msg}");
        assert!(msg.contains("fresh data directory"), "{msg}");
        assert!(msg.contains(&format!("{ops} ops")), "{msg}");
    }

    // Format 1 — never stamped at all, and not empty, so it is somebody's
    // real history rather than a directory that has never been used.
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let (node, genesis, _) = make(dir.path(), None);
        let err = StateEngine::open(dir.path(), node, genesis).expect_err("format 1 is refused");
        assert!(err.to_string().contains("predates protocol 2.0"), "{err}");
    }

    // **And an unstamped *empty* log is a fresh node, not a refusal.** The
    // stamp is written on first use, so every new install passes through
    // the same branch a pre-2.0 directory does — and a rule that cannot
    // tell them apart bricks `meshnode up` on a machine that has never run
    // it.
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let founder_user = Keypair::generate().expect("user key");
        let founder_node = Keypair::generate().expect("node key");
        let genesis =
            Genesis::create("f0-fresh", &founder_user, founder_node.node_id()).expect("genesis");
        {
            let engine = StateEngine::open(dir.path(), founder_node.clone(), genesis.clone())
                .expect("a fresh dir opens");
            drop(engine);
        }
        let db = Database::create(dir.path().join("state/oplog.redb")).expect("open raw");
        let txn = db.begin_write().expect("write txn");
        {
            let mut meta = txn.open_table(META).expect("meta");
            meta.remove("format").expect("unstamp");
        }
        txn.commit().expect("commit");
        {
            let read = db.begin_read().expect("read txn");
            let ops = read.open_table(OPS).expect("ops");
            assert_eq!(ops.len().expect("len"), 0, "the fixture must be empty");
        }
        // redb holds an exclusive file lock, so the raw handle has to go
        // before the engine can take the same file back.
        drop(db);
        StateEngine::open(dir.path(), founder_node, genesis)
            .expect("an unstamped empty log is a fresh node");
    }
}

/// `missing_for` is a per-author range pull, not a full decode — and it
/// still answers exactly what a peer lacks, including for a peer that has
/// never heard of an author at all.
#[test]
fn missing_for_answers_per_author_ranges() {
    use mesh_core::genesis::Genesis;
    use mesh_core::identity::Keypair;

    let dir = tempfile::tempdir().expect("tempdir");
    let founder_user = Keypair::generate().expect("user key");
    let founder_node = Keypair::generate().expect("node key");
    let genesis =
        Genesis::create("f0-range", &founder_user, founder_node.node_id()).expect("genesis");
    let engine = StateEngine::open(dir.path(), founder_node.clone(), genesis).expect("open");

    for i in 0..12u32 {
        engine
            .author_op(OpBody::MemberAdd {
                member: MemberInfo {
                    node_id: founder_node.node_id(),
                    name: format!("n{i}"),
                    addrs: vec![],
                    tags: vec![],
                    slots: 1,
                    joined_at_micros: mesh_proto::now_micros(),
                },
                invite: None,
            })
            .expect("author");
    }
    let me = founder_node.node_id();

    // A peer that has nothing gets everything.
    let (all, more) = engine.missing_for(&vec![]).expect("missing");
    assert_eq!(all.len(), 12);
    assert!(!more);

    // A peer that is partway gets the tail, in sequence order.
    let (tail, _) = engine.missing_for(&vec![(me, 9)]).expect("missing");
    assert_eq!(
        tail.iter().map(|o| o.seq).collect::<Vec<_>>(),
        vec![10, 11, 12]
    );

    // A caught-up peer gets nothing — and an *ahead* peer gets nothing
    // rather than a negative range.
    assert!(
        engine
            .missing_for(&vec![(me, 12)])
            .expect("missing")
            .0
            .is_empty()
    );
    assert!(
        engine
            .missing_for(&vec![(me, 99)])
            .expect("missing")
            .0
            .is_empty()
    );

    // An author the peer names but we have never seen is simply not ours
    // to serve.
    let stranger = Keypair::generate().expect("stranger");
    let (some, _) = engine
        .missing_for(&vec![(stranger.node_id(), 5)])
        .expect("missing");
    assert_eq!(some.len(), 12, "our own ops are still owed");
}

/// A backup taken before F0 restores into a 1.7 node. The op bodies are
/// re-wrapped, not re-signed — the bytes a signature covered are the bytes
/// the envelope carries.
#[test]
fn a_pre_f0_backup_restores() {
    use mesh_core::backup;
    use mesh_core::genesis::Genesis;
    use mesh_core::identity::Identity;

    let src = tempfile::tempdir().expect("tempdir");
    let identity = Identity::load_or_generate(src.path()).expect("identity");
    let genesis = Genesis::create(
        "f0-backup",
        identity
            .user
            .as_ref()
            .expect("a founded node has a user key"),
        identity.node.node_id(),
    )
    .expect("genesis");
    genesis.save(src.path()).expect("save genesis");
    let engine =
        StateEngine::open(src.path(), identity.node.clone(), genesis.clone()).expect("open");
    engine
        .author_op(OpBody::MemberAdd {
            member: MemberInfo {
                node_id: identity.node.node_id(),
                name: "alpha".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
                joined_at_micros: mesh_proto::now_micros(),
            },
            invite: None,
        })
        .expect("author");
    let ops = engine.all_ops().expect("all_ops");
    drop(engine);

    // Hand-write a format-1 document: the ops are a postcard `Vec<Op>`.
    let out = src.path().join("legacy.json");
    backup::write_backup(&out, src.path(), "alpha", &genesis, ops.clone()).expect("backup");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&out).expect("read")).expect("parse");
    let legacy: Vec<mesh_proto::Op> = ops.iter().map(|o| o.to_op().expect("known")).collect();
    doc["format"] = serde_json::json!(1);
    doc["ops_b64"] = serde_json::json!(
        data_encoding::BASE64.encode(&postcard::to_stdvec(&legacy).expect("encode legacy"))
    );
    std::fs::write(&out, serde_json::to_vec_pretty(&doc).expect("encode")).expect("write");

    let dest = tempfile::tempdir().expect("tempdir");
    let dest_dir = dest.path().join("restored");
    let report = backup::restore(&out, &dest_dir).expect("restore a pre-F0 backup");
    assert_eq!(report.ops, legacy.len() as u64);
    assert_eq!(report.mesh_name, "f0-backup");
    assert_eq!(
        report.members, 1,
        "state materialized from the restored log"
    );
}

// ---------------------------------------------------------------------------
// F0.4: the sync path no longer scales with log size
// ---------------------------------------------------------------------------

/// Build a log of `n` ops in one transaction, authored by `key`, and return
/// the engine holding it. One batch, not `n` calls: `author_op` commits per
/// op, and the point here is the shape of the *query*, not the write.
fn engine_with_log(
    dir: &std::path::Path,
    key: &mesh_core::identity::Keypair,
    genesis: &mesh_core::genesis::Genesis,
    n: u64,
) -> std::sync::Arc<StateEngine> {
    use mesh_testkit::ops::seal;

    let engine = StateEngine::open(dir, key.clone(), genesis.clone()).expect("open");
    // Seq 1 makes the author a member; everything after is a cheap update.
    engine
        .author_op(OpBody::MemberAdd {
            member: MemberInfo {
                node_id: key.node_id(),
                name: "bulk".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
                joined_at_micros: mesh_proto::now_micros(),
            },
            invite: None,
        })
        .expect("self add");

    let batch: Vec<OpEnvelope> = (2..=n)
        .map(|seq| {
            let hlc = mesh_proto::Hlc {
                wall_micros: seq,
                logical: 0,
            };
            seal(
                key,
                seq,
                hlc,
                &OpBody::MemberUpdate {
                    node_id: key.node_id(),
                    name: "bulk".into(),
                    addrs: vec![],
                    tags: vec![],
                    slots: 1,
                },
            )
        })
        .collect();
    assert_eq!(
        engine.ingest(batch).expect("bulk ingest"),
        n as usize - 1,
        "the fixture log must apply in full"
    );
    engine
}

/// The latent scale bug F0.4 closes (§2.2): `missing_for` used to decode
/// **every op in the log** on every gossip tick, per peer, and then filter
/// in memory. It is now a per-author range pull, so answering "you are
/// missing one op" costs one op.
///
/// Asserted as wall clock with an enormous margin rather than as a
/// microbenchmark: the old shape would do ~50 million decodes here and the
/// new one does ~5,000, so any bound between them separates the two
/// unambiguously and nothing about machine speed can flip the verdict.
#[test]
fn the_sync_path_does_not_scale_with_log_size() {
    use mesh_core::genesis::Genesis;
    use mesh_core::identity::Keypair;
    use std::time::Instant;

    const OPS: u64 = 10_000;
    const QUERIES: u64 = 5_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let founder_user = Keypair::generate().expect("user key");
    let node = Keypair::generate().expect("node key");
    let genesis = Genesis::create("f0-scale", &founder_user, node.node_id()).expect("genesis");
    let engine = engine_with_log(dir.path(), &node, &genesis, OPS);
    let me = node.node_id();

    // A peer one op behind: the answer is one op, whatever the log holds.
    let start = Instant::now();
    for _ in 0..QUERIES {
        let (ops, more) = engine.missing_for(&vec![(me, OPS - 1)]).expect("missing");
        assert_eq!(ops.len(), 1);
        assert!(!more);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "{QUERIES} near-caught-up sync queries over a {OPS}-op log took {elapsed:?} — \
         that is the shape of a full-log decode per query, which F0.4 removed"
    );

    // A peer that is far behind is still bounded by the batch cap, not by
    // the log: the requester loops on `more`.
    let (batch, more) = engine.missing_for(&vec![]).expect("missing");
    assert_eq!(batch.len(), mesh_core::store::SYNC_BATCH);
    assert!(more, "a log larger than one batch must say so");
}
