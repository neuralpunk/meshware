//! M18 facts (`docs/M17-M21-FABRIC-KNOWLEDGE-AND-AGENTS.md` §4): a source of
//! truth built by observation.
//!
//! The canonical encoding, the flap guard's arithmetic, the comparison
//! operators and the collector parser are unit tests in `mesh_core::facts`,
//! because they are pure functions and a mesh adds nothing to them. What is
//! tested here is what a *mesh* has to get right:
//!
//! - a brand-new node publishes `system` facts within one interval of
//!   joining, and every other node can read them;
//! - a stable machine authors **one op per collector, ever** — the property
//!   the whole storage argument rests on;
//! - `--on 'fact.os_id=…'` hits exactly the matching boxes, and a node with
//!   no such fact matches **nothing** rather than everything;
//! - only the subject may speak for the subject;
//! - the byte cap refuses on the node that produced it, naming the count;
//! - a collector that emits a measurement is reported flapping, **with the
//!   key named**.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, Liveness, OpBody};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

/// Wait until `node` has a `system` document for `subject`.
async fn wait_for_facts(node: &TestNode, subject: mesh_proto::NodeId) -> bool {
    wait_until(Duration::from_secs(30), || {
        node.node.engine().read(|s| {
            s.facts
                .get(&mesh_proto::FactSubject::Node(subject))
                .is_some_and(|by| by.contains_key(mesh_proto::SYSTEM_COLLECTOR))
        })
    })
    .await
}

fn fact_of(node: &TestNode, subject: mesh_proto::NodeId, key: &str) -> Option<String> {
    node.node.engine().read(|s| {
        s.facts
            .get(&mesh_proto::FactSubject::Node(subject))
            .and_then(|by| by.get(mesh_proto::SYSTEM_COLLECTOR))
            .and_then(|e| e.get(key))
    })
}

/// How many `FactsPublish` ops this node's log holds for a collector.
fn publish_count(node: &TestNode, collector: &str) -> usize {
    node.node
        .engine()
        .fact_observations()
        .expect("read the log")
        .iter()
        .filter(|(_, name, _, _)| name == collector)
        .count()
}

// ---------------------------------------------------------------------------
// P1 — the entity and the built-in collector
// ---------------------------------------------------------------------------

/// The headline: a node describes itself, with nobody publishing anything.
#[tokio::test]
async fn a_fresh_node_publishes_its_own_facts() {
    let founder = spawn_founder("alpha", "facts-mesh").await;
    let me = founder.node.node_id();

    assert!(
        wait_for_facts(&founder, me).await,
        "no system facts appeared"
    );

    // Every fact the built-in collector can promise on any Linux box.
    assert_eq!(
        fact_of(&founder, me, "arch").as_deref(),
        Some(std::env::consts::ARCH)
    );
    assert!(fact_of(&founder, me, "meshware_version").is_some());
    // And nothing that is a measurement (§4.2).
    for banned in ["uptime", "load1", "memory_free_mb", "disk_free_mb"] {
        assert!(
            fact_of(&founder, me, banned).is_none(),
            "`{banned}` is a measurement, and publishing one would author an op every \
             interval on every node forever"
        );
    }
}

/// A stable machine authors **one op per collector, ever**. If this ever
/// fails, the storage argument in §4.4 fails with it.
#[tokio::test]
async fn a_stable_machine_publishes_once() {
    let founder = spawn_founder("alpha", "facts-stable").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);

    let first = publish_count(&founder, mesh_proto::SYSTEM_COLLECTOR);
    assert_eq!(first, 1, "the first observation is one op");

    // Several collector ticks' worth of wall clock. The machine has not
    // changed, so neither has the digest, so nothing is authored.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        publish_count(&founder, mesh_proto::SYSTEM_COLLECTOR),
        first,
        "an unchanged machine must author nothing"
    );
}

/// Facts survive a restart and are read back off the log, not re-derived —
/// which is the property that makes them selectable when a node is offline.
#[tokio::test]
async fn facts_survive_a_restart() {
    let founder = spawn_founder("alpha", "facts-restart").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let before = fact_of(&founder, me, "arch");

    let founder = founder.restart().await;
    let back = wait_until(Duration::from_secs(20), || {
        fact_of(&founder, me, "arch").is_some()
    })
    .await;
    assert!(back, "facts did not come back after a restart");
    assert_eq!(fact_of(&founder, me, "arch"), before);
    // And the restart did not author a second op: the digest is unchanged.
    assert_eq!(publish_count(&founder, mesh_proto::SYSTEM_COLLECTOR), 1);
}

/// A second node's facts reach the first — they are ordinary replicated
/// state, which is the whole reason they are in the op-log.
#[tokio::test]
async fn facts_replicate_to_every_node() {
    let founder = spawn_founder("alpha", "facts-replicate").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();

    assert!(
        wait_for_facts(&founder, beta_id).await,
        "beta's facts never reached alpha"
    );
    assert_eq!(
        fact_of(&founder, beta_id, "arch").as_deref(),
        Some(std::env::consts::ARCH)
    );
    drop(beta);
}

/// **Only the subject may speak for the subject** (§4.4). There is no
/// founder exception: nobody needs to author another machine's facts, and
/// an exception would be a way to lie about a fleet.
#[tokio::test]
async fn one_node_cannot_publish_another_nodes_facts() {
    let founder = spawn_founder("alpha", "facts-authority").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    assert!(wait_for_facts(&founder, beta_id).await);

    let lie = br#"{"os_id":"totally-not-what-beta-runs"}"#;
    founder
        .node
        .engine()
        .author_op(OpBody::FactsPublish {
            subject: mesh_proto::FactSubject::Node(beta_id),
            collector: mesh_proto::SYSTEM_COLLECTOR.to_string(),
            doc: lie.to_vec(),
            digest: *blake3::hash(lie).as_bytes(),
            collected_at_micros: mesh_proto::now_micros(),
        })
        .expect("authoring is allowed; applying is not");

    // The op applies as a no-op on every node, including its author.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(
        fact_of(&founder, beta_id, "os_id").as_deref(),
        Some("totally-not-what-beta-runs"),
        "alpha must not be able to speak for beta"
    );
    drop(beta);
}

/// A document whose digest does not cover it is dropped, whoever authored
/// it — the cheapest integrity check in the milestone.
#[tokio::test]
async fn a_document_that_does_not_match_its_digest_is_dropped() {
    let founder = spawn_founder("alpha", "facts-digest").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);

    founder
        .node
        .engine()
        .author_op(OpBody::FactsPublish {
            subject: mesh_proto::FactSubject::Node(me),
            collector: "forged".to_string(),
            doc: br#"{"a":1}"#.to_vec(),
            digest: [0u8; 32],
            collected_at_micros: mesh_proto::now_micros(),
        })
        .expect("authored");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let has = founder.node.engine().read(|s| {
        s.facts
            .get(&mesh_proto::FactSubject::Node(me))
            .is_some_and(|by| by.contains_key("forged"))
    });
    assert!(!has, "a mismatched digest must be refused");
}

/// The cap refuses on the node that produced it, and **names the byte
/// count**. There is no spill-to-object path on purpose: a document that
/// does not fit is a collector bug, and giving it somewhere to go would
/// mean the fix never happens (§4.4).
#[tokio::test]
async fn an_oversized_document_is_refused_where_it_was_produced() {
    let founder = spawn_founder("alpha", "facts-cap").await;
    let user = founder_user_key(&founder).clone();

    let source = format!(
        "collector: fat\non: any\nevery: 60s\ncommand: |-\n  \
         python3 -c 'print(\"{{\\\"blob\\\": \\\"\" + \"x\"*{} + \"\\\"}}\")'\n",
        mesh_proto::MAX_FACT_DOC + 100
    );
    let collector = mesh_core::facts::parse_collector(&source, user.user_id()).expect("parse");
    let signed = mesh_core::facts::sign_collector(&user, collector);
    match ipc_as(
        &founder,
        &user,
        IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        },
    )
    .await
    {
        IpcResponse::Ok => {}
        other => panic!("publishing the collector: {other:?}"),
    }

    // The refusal happens where the document is produced, so nothing ever
    // reaches replicated state.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let published = founder
        .node
        .engine()
        .read(|s| s.facts.values().any(|by| by.contains_key("fat")));
    assert!(!published, "an oversized document must never be published");

    // And the refusal itself names the count, checked on the pure path.
    let big = serde_json::json!({"blob": "x".repeat(mesh_proto::MAX_FACT_DOC + 100)});
    let why = mesh_core::facts::prepare("fat", &big.to_string(), None)
        .unwrap_err()
        .to_string();
    assert!(why.contains(&mesh_proto::MAX_FACT_DOC.to_string()), "{why}");
}

/// **A consequence worth writing down.** Every fact publish is an `OpBody`
/// index a pre-1.7 build has no variant for, and every node authors one at
/// boot — so after M18 a member that has published facts can no longer
/// admit a pre-1.7 joiner (F0 §3.3). The escape hatch is `[facts] collect =
/// "off"` on whichever member holds the invite, and the refusal names both
/// remedies rather than timing out.
#[tokio::test(flavor = "multi_thread")]
async fn a_publishing_member_can_no_longer_admit_a_pre_f0_joiner() {
    use mesh_core::membership;

    let founder = spawn_founder("alpha", "facts-f0").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await, "no facts to block with");

    let token = membership::create_invite(
        founder.node.engine(),
        founder_user_key(&founder),
        membership::InviteParams {
            ttl_secs: 3600,
            uses: 10,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .expect("invite");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = mesh_testkit::test_config_for(dir.path(), "old", "ignored", vec![]);
    config.proto_minor_override = Some(mesh_proto::OPLOG_MINOR - 1);
    let why = membership::join_mesh(&config, &token.encode(), &[])
        .await
        .expect_err("a publishing member must refuse a pre-1.7 joiner")
        .to_string();
    assert!(why.contains("older than 1.7"), "{why}");
    assert!(
        why.contains("Upgrade") || why.contains("join through"),
        "the refusal must name a remedy: {why}"
    );

    // And the escape hatch works: the same joiner gets in through a member
    // that is not publishing.
    let quiet = mesh_testkit::spawn_founder_with("quiet", "facts-f0-quiet", vec![], |c| {
        c.facts.collect = false
    })
    .await;
    let token = membership::create_invite(
        quiet.node.engine(),
        founder_user_key(&quiet),
        membership::InviteParams {
            ttl_secs: 3600,
            uses: 10,
            tags: vec![],
            bootstrap: vec![quiet.addr_string()],
        },
    )
    .expect("invite");
    let dir2 = tempfile::tempdir().expect("tempdir");
    let mut config2 = mesh_testkit::test_config_for(dir2.path(), "old2", "ignored", vec![]);
    config2.proto_minor_override = Some(mesh_proto::OPLOG_MINOR - 1);
    membership::join_mesh(&config2, &token.encode(), &[])
        .await
        .expect("a member with facts off can still admit a pre-1.7 joiner");
}

// ---------------------------------------------------------------------------
// P2 — custom collectors
// ---------------------------------------------------------------------------

/// A collector is a script that prints one JSON object. That is the whole
/// contract (§4.5).
#[tokio::test]
async fn a_custom_collector_publishes_what_it_prints() {
    let founder = spawn_founder("alpha", "facts-custom").await;
    let user = founder_user_key(&founder).clone();
    let me = founder.node.node_id();

    let collector = mesh_core::facts::parse_collector(
        "collector: fleet\non: any\nevery: 60s\ncommand: \"echo '{\\\"owner\\\":\\\"platform\\\",\\\"tier\\\":2}'\"\n",
        user.user_id(),
    )
    .expect("parse");
    let signed = mesh_core::facts::sign_collector(&user, collector);
    assert!(matches!(
        ipc_as(
            &founder,
            &user,
            IpcRequest::FactCollectorPut {
                collector: Box::new(signed)
            }
        )
        .await,
        IpcResponse::Ok
    ));

    let published = wait_until(Duration::from_secs(40), || {
        founder.node.engine().read(|s| {
            s.facts
                .get(&mesh_proto::FactSubject::Node(me))
                .is_some_and(|by| by.contains_key("fleet"))
        })
    })
    .await;
    assert!(published, "the custom collector never published");

    let owner = founder.node.engine().read(|s| {
        s.facts
            .get(&mesh_proto::FactSubject::Node(me))
            .and_then(|by| by.get("fleet"))
            .and_then(|e| e.get("owner"))
    });
    assert_eq!(owner.as_deref(), Some("platform"));
}

/// The built-in name is reserved, and a collector that names it is refused
/// where somebody is watching.
#[tokio::test]
async fn the_built_in_collector_name_is_reserved() {
    let founder = spawn_founder("alpha", "facts-reserved").await;
    let user = founder_user_key(&founder).clone();

    // The client-side parser refuses it first.
    let why = mesh_core::facts::parse_collector(
        "collector: system\non: any\ncommand: \"echo {}\"\n",
        user.user_id(),
    )
    .unwrap_err()
    .to_string();
    assert!(why.contains("built-in"), "{why}");

    // And so does the daemon, for a client that skipped the parser.
    let mut forged = mesh_core::facts::parse_collector(
        "collector: fine\non: any\ncommand: \"echo {}\"\n",
        user.user_id(),
    )
    .expect("parse");
    forged.name = mesh_proto::SYSTEM_COLLECTOR.into();
    let signed = mesh_core::facts::sign_collector(&user, forged);
    match ipc_as(
        &founder,
        &user,
        IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        },
    )
    .await
    {
        IpcResponse::Error { message } => assert!(message.contains("built-in"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Pausing is an edit, and it is inside the signature — so there is no verb
/// that changes what a fleet reports without a signature behind it. This is
/// also why M18 needed two op indices rather than three.
#[tokio::test]
async fn pausing_a_collector_stops_it_without_a_tombstone() {
    let founder = spawn_founder("alpha", "facts-pause").await;
    let user = founder_user_key(&founder).clone();

    let collector = mesh_core::facts::parse_collector(
        "collector: paused-one\non: any\nevery: 60s\npaused: true\ncommand: \"echo '{\\\"x\\\":1}'\"\n",
        user.user_id(),
    )
    .expect("parse");
    assert!(collector.paused);
    let signed = mesh_core::facts::sign_collector(&user, collector);
    assert!(matches!(
        ipc_as(
            &founder,
            &user,
            IpcRequest::FactCollectorPut {
                collector: Box::new(signed)
            }
        )
        .await,
        IpcResponse::Ok
    ));

    tokio::time::sleep(Duration::from_secs(2)).await;
    let published = founder
        .node
        .engine()
        .read(|s| s.facts.values().any(|by| by.contains_key("paused-one")));
    assert!(!published, "a paused collector must run nothing");

    // It is still in state, with its record intact — no tombstone.
    let listed = founder
        .node
        .engine()
        .read(|s| s.collectors.contains_key("paused-one"));
    assert!(listed, "a paused collector stays in state");
}

// ---------------------------------------------------------------------------
// P3 — fact selectors
// ---------------------------------------------------------------------------

/// The exit demo: a fact selector hits exactly the matching boxes.
#[tokio::test]
async fn a_fact_selector_targets_exactly_the_matching_nodes() {
    let founder = spawn_founder("alpha", "facts-select").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let user = founder_user_key(&founder).clone();

    let arch = std::env::consts::ARCH;
    match ipc_as(
        &founder,
        &user,
        IpcRequest::TargetPreview {
            selector: format!("fact.arch={arch}"),
        },
    )
    .await
    {
        IpcResponse::Nodes(rows) => {
            assert_eq!(rows.len(), 1, "this node runs {arch}");
            assert_eq!(rows[0].name, "alpha");
        }
        other => panic!("target preview: {other:?}"),
    }

    // A fact nobody has matches **nothing**, not everything. This is the
    // fail-closed direction, and it is the whole safety argument for
    // adding a selector variant at all.
    match ipc_as(
        &founder,
        &user,
        IpcRequest::TargetPreview {
            selector: "fact.os_id=an-os-that-does-not-exist".into(),
        },
    )
    .await
    {
        IpcResponse::Nodes(rows) => assert!(rows.is_empty(), "{rows:?}"),
        other => panic!("target preview: {other:?}"),
    }

    // And so does a malformed one, which is exactly how a pre-M18 node
    // reads *any* fact selector.
    match ipc_as(
        &founder,
        &user,
        IpcRequest::TargetPreview {
            selector: "fact.kernel_release".into(),
        },
    )
    .await
    {
        IpcResponse::Nodes(rows) => {
            assert!(rows.is_empty(), "a malformed selector matches nothing")
        }
        other => panic!("target preview: {other:?}"),
    }
}

/// A pre-M18 node reads `fact.…` as a node *name* and matches nothing —
/// the degradation §4.7 depends on, checked against the real parser.
#[test]
fn a_fact_selector_degrades_closed() {
    use mesh_core::membership::Selector;
    let member = mesh_proto::MemberInfo {
        node_id: mesh_proto::NodeId([1u8; 32]),
        name: "hermes".into(),
        addrs: vec![],
        tags: vec!["prod".into()],
        slots: 4,
        joined_at_micros: 0,
    };
    // The M18 form, judged without state: false, because facts are not on
    // the member record and a caller that has only the record cannot know.
    let sel = Selector::parse("fact.os_id=debian");
    assert!(sel.needs_facts());
    assert!(!sel.matches(&member));
    // The pre-M18 reading of the same string.
    assert!(
        !Selector::Name("fact.os_id=debian".into()).matches(&member),
        "an old node reads it as a name and matches nothing"
    );
}

/// `facts query` answers across the mesh, and says which subjects have no
/// such key rather than quietly omitting them.
#[tokio::test]
async fn a_fact_query_reports_matches_and_unknowns() {
    let founder = spawn_founder("alpha", "facts-query").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let user = founder_user_key(&founder).clone();

    match ipc_as(
        &founder,
        &user,
        IpcRequest::FactsQuery {
            expression: format!("arch={}", std::env::consts::ARCH),
            collector: None,
        },
    )
    .await
    {
        IpcResponse::FactsMatched {
            subjects, unknown, ..
        } => {
            assert_eq!(subjects.len(), 1);
            assert_eq!(subjects[0].0, "alpha");
            assert!(unknown.is_empty(), "{unknown:?}");
        }
        other => panic!("query: {other:?}"),
    }

    match ipc_as(
        &founder,
        &user,
        IpcRequest::FactsQuery {
            expression: "nonexistent_key=whatever".into(),
            collector: None,
        },
    )
    .await
    {
        IpcResponse::FactsMatched {
            subjects, unknown, ..
        } => {
            assert!(subjects.is_empty());
            assert_eq!(
                unknown,
                vec!["alpha".to_string()],
                "the unknown must be named"
            );
        }
        other => panic!("query: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P5 — history
// ---------------------------------------------------------------------------

/// `facts diff` reads the op-log through the derived index — which is what
/// makes deleting the index cost one slow start and nothing else.
#[tokio::test]
async fn fact_history_is_queryable() {
    let founder = spawn_founder("alpha", "facts-history").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let user = founder_user_key(&founder).clone();

    let indexed = wait_until(Duration::from_secs(20), || true).await;
    assert!(indexed);
    tokio::time::sleep(Duration::from_secs(3)).await;

    match ipc_as(
        &founder,
        &user,
        IpcRequest::FactsDiff {
            subject: "alpha".into(),
            collector: None,
            since_micros: None,
            limit: 50,
        },
    )
    .await
    {
        IpcResponse::FactsDiff(rows) => {
            assert!(
                !rows.is_empty(),
                "the baseline observation must be reported"
            );
            let baseline = &rows[0];
            assert_eq!(baseline.collector, mesh_proto::SYSTEM_COLLECTOR);
            assert!(
                baseline.changes.iter().any(|c| c.key == "arch"),
                "the baseline names every key as it first appeared: {:?}",
                baseline.changes
            );
        }
        other => panic!("diff: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// `meshnode facts` with no argument means this node, and names it.
#[tokio::test]
async fn facts_reads_answer_by_name() {
    let founder = spawn_founder("alpha", "facts-read").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let user = founder_user_key(&founder).clone();

    for subject in ["", "alpha"] {
        match ipc_as(
            &founder,
            &user,
            IpcRequest::Facts {
                subject: subject.into(),
                collector: Some(mesh_proto::SYSTEM_COLLECTOR.into()),
            },
        )
        .await
        {
            IpcResponse::Facts(rows) => {
                assert_eq!(rows.len(), 1, "subject {subject:?}");
                assert_eq!(rows[0].subject_name, "alpha");
                assert_eq!(rows[0].author_name, "alpha");
                assert!(rows[0].collected_at_micros > 0);
            }
            other => panic!("facts {subject:?}: {other:?}"),
        }
    }

    match ipc_as(
        &founder,
        &user,
        IpcRequest::Facts {
            subject: "nobody".into(),
            collector: None,
        },
    )
    .await
    {
        IpcResponse::Error { message } => assert!(message.contains("no node named"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The built-in collector is listed, and it is listed as built in — an
/// operator accounting for what runs on their fleet has to be able to see
/// the one nobody published.
#[tokio::test]
async fn the_built_in_collector_is_listed() {
    let founder = spawn_founder("alpha", "facts-list").await;
    let me = founder.node.node_id();
    assert!(wait_for_facts(&founder, me).await);
    let user = founder_user_key(&founder).clone();

    match ipc_as(&founder, &user, IpcRequest::FactCollectors).await {
        IpcResponse::FactCollectors(rows) => {
            let system = rows
                .iter()
                .find(|r| r.name == mesh_proto::SYSTEM_COLLECTOR)
                .expect("the built-in collector is listed");
            assert!(system.built_in);
            assert!(!system.paused);
            assert_eq!(system.subjects, 1);
            assert!(system.flapping_keys.is_empty());
        }
        other => panic!("collectors: {other:?}"),
    }
}

/// Two nodes converge on the same facts, and a node that never ran the
/// collector still carries and serves the document.
#[tokio::test]
async fn every_node_converges_on_the_same_documents() {
    let founder = spawn_founder("alpha", "facts-converge").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let (a, b) = (founder.node.node_id(), beta.node.node_id());

    let ready = wait_until(Duration::from_secs(30), || {
        [(&founder, a), (&founder, b), (&beta, a), (&beta, b)]
            .iter()
            .all(|(node, id)| {
                node.node.engine().read(|s| {
                    s.facts
                        .get(&mesh_proto::FactSubject::Node(*id))
                        .is_some_and(|by| by.contains_key(mesh_proto::SYSTEM_COLLECTOR))
                })
            })
    })
    .await;
    assert!(ready, "facts did not converge");

    for id in [a, b] {
        let from_alpha = founder.node.engine().read(|s| {
            s.facts[&mesh_proto::FactSubject::Node(id)][mesh_proto::SYSTEM_COLLECTOR].digest
        });
        let from_beta = beta.node.engine().read(|s| {
            s.facts[&mesh_proto::FactSubject::Node(id)][mesh_proto::SYSTEM_COLLECTOR].digest
        });
        assert_eq!(from_alpha, from_beta, "digests must agree byte for byte");
    }
    let _ = Liveness::Alive;
}

/// A revoked node keeps its member record forever — the tombstone has to
/// persist or the revoked key could rejoin — so a machine that is revoked
/// and then re-paired leaves **two members carrying one name**. Asking for
/// that name must answer about the live one.
///
/// Found in the field: `meshnode facts APUS` and Director's Facts panel
/// both reported "no facts yet" about a node that had published a perfectly
/// good document, because the lookup took whichever record the map yielded
/// first and that was the tombstone. Nothing anywhere said why, and every
/// symptom pointed at the collector rather than at the resolver.
///
/// `members` is a `BTreeMap` keyed by node id, so which of two same-named
/// records comes first is decided by two random keypairs. The test revokes
/// **whichever one iteration reaches first**, which is the only way to put
/// the tombstone in front of the live record every run rather than half of
/// them — without that, this passes against the bug it exists to catch.
#[tokio::test]
async fn a_name_shared_with_a_revoked_node_resolves_to_the_live_one() {
    let founder = spawn_founder("alpha", "facts-shadow").await;
    let admin = founder_user_key(&founder).clone();

    // Two members, one name: the machine was re-paired and minted a fresh
    // identity, exactly as a revoke-then-rejoin does in the field.
    let one = join_new_node("beta", &founder, vec![]).await;
    let two = join_new_node("beta", &founder, vec![]).await;
    let (one_id, two_id) = (one.node.node_id(), two.node.node_id());
    assert_ne!(one_id, two_id);
    assert!(wait_for_facts(&founder, one_id).await);
    assert!(wait_for_facts(&founder, two_id).await);

    // Whoever the resolver would reach first is the one we kill, so the
    // dead record is always the one shadowing the live one.
    let first = founder
        .node
        .engine()
        .read(|s| {
            s.members
                .values()
                .find(|m| m.info.name == "beta")
                .map(|m| m.info.node_id)
        })
        .expect("a member named beta");
    let live_id = if first == one_id { two_id } else { one_id };

    let mut revocation = mesh_proto::NodeRevocation {
        node_id: first,
        reason: "re-imaging the box".into(),
        by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = admin.sign(&revocation.signing_bytes()).to_vec();
    match ipc_as(&founder, &admin, IpcRequest::NodeRevoke { revocation }).await {
        IpcResponse::Ok => {}
        other => panic!("revoke refused: {other:?}"),
    }

    let rows = match ipc_as(
        &founder,
        &admin,
        IpcRequest::Facts {
            subject: "beta".into(),
            collector: None,
        },
    )
    .await
    {
        IpcResponse::Facts(rows) => rows,
        other => panic!("facts by name failed: {other:?}"),
    };
    assert!(
        rows.iter()
            .all(|r| r.subject == mesh_proto::FactSubject::Node(live_id)),
        "the name resolved to the revoked record, which has no facts and never will; got {:?}",
        rows.iter().map(|r| r.subject.label()).collect::<Vec<_>>()
    );
    assert!(!rows.is_empty(), "the live node's document went missing");
    drop(one);
    drop(two);
}
