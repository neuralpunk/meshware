//! What a sink destination is allowed to cost, and what it must admit to.
//!
//! Two things live here that are not about any one destination. The first
//! is the **dependency fence**: every destination this tree has ever taken
//! was built without adding a driver to the daemon, and the assertion below
//! is what keeps that true — a Postgres client or a columnar engine in
//! `meshnode` would arrive as somebody's convenient refactor rather than as
//! a decision. The second is that a destination **says what it cannot do
//! before anybody picks it**, which is §14.4 rule 1 and the reason the
//! capability triple sits on the enum rather than in a UI table.
//!
//! Four destinations were retired on 2026-09-20 and their variants stayed —
//! `SinkKind` is a signed field of a replicated op and postcard encodes an
//! enum by index. A retired kind that still *behaved* would be the worse
//! outcome, so the last test here pins the refusal instead.
#![allow(clippy::unwrap_used)]

use mesh_proto::SinkKind;

/// **The fence, and it is the deliverable.**
///
/// "The most-requested destination, built the way that does not cost the
/// four-dependency property" is a claim about the *daemon's* dependency
/// tree, and this is the assertion that keeps it. Whatever needs one of
/// these belongs in `mesh-archive` or in a plugin (§4 decision 1).
#[test]
fn the_node_binary_carries_no_database_driver_and_no_columnar_engine() {
    let tree = std::process::Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "meshware-node",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree");
    assert!(
        tree.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let tree = String::from_utf8_lossy(&tree.stdout).to_ascii_lowercase();

    for forbidden in [
        // B5's fence, kept after the Postgres destination went: the point
        // was never that one connector behaved, it was that no connector
        // may put a driver in the daemon.
        "sqlx",
        // B4's, which arrived with the bucket.
        "object_store",
        "reqwest",
        // C2's, asserted here too so all three live in one place.
        "datafusion",
        "arrow ",
        "parquet",
    ] {
        assert!(
            !tree.contains(forbidden),
            "`{forbidden}` is in meshware-node's dependency tree — the daemon is one small \
             binary you can drop on anything, and that is the property this test exists to \
             keep. Whatever needs it belongs in `mesh-archive` or in a plugin (§4 decision 1)."
        );
    }
}

/// Which destinations the mesh can govern, and which it can only watch.
///
/// **Not the same question as `is_http`**, which is exactly the case a
/// control written around that would wave through. A file and a plugin do
/// not leave the machine as far as meshware is concerned: it cannot govern
/// a subprocess's network, and pretending to would be worse than saying so.
#[test]
fn a_destination_that_leaves_the_machine_says_so_and_a_local_one_does_not() {
    for kind in [
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::Webhook,
    ] {
        assert!(kind.leaves_the_machine(), "{kind:?}");
    }
    assert!(!SinkKind::Plugin.leaves_the_machine());
}

/// §14.4 rule 1: the picker cannot promise a read that does not exist.
///
/// A destination that cannot hand one job's output back must say so in the
/// copy somebody reads *before* they choose, not in an empty pane six
/// months later — and `replay_state` has to name what is missing rather
/// than reading as a flat no.
#[test]
fn a_destination_says_what_it_cannot_do_before_selection() {
    // A webhook is somebody else's URL. Nothing can be read back from it,
    // and the copy has to lead with that rather than with the throughput.
    assert!(!SinkKind::Webhook.capabilities().replay_payloads);
    assert_eq!(
        SinkKind::Webhook.replay_state(true),
        mesh_proto::ReplayState::NotSupported
    );
    let copy = SinkKind::Webhook.capability_sentence().to_ascii_lowercase();
    assert!(
        copy.contains("cannot read it back") || copy.contains("cannot be read"),
        "the picker copy hides the limitation: {copy}"
    );

    // The two that *can* hand a payload back say so, because the whole
    // point of the triple is that it is not decoration.
    for kind in [SinkKind::ClickHouse, SinkKind::VictoriaLogs] {
        assert!(kind.capabilities().replay_payloads, "{kind:?}");
        assert!(kind.capabilities().archives, "{kind:?}");
    }

    // **`browse_records` is false everywhere, and that is §18 fence 2, not
    // a gap.** The History index is answered from local tiers, always; the
    // moment a pane is painted from somebody's warehouse we have shipped a
    // control plane. A destination that flipped this to true would be
    // claiming the one thing no sink is allowed to be for.
    for kind in [
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::Webhook,
        SinkKind::Plugin,
    ] {
        assert!(!kind.capabilities().browse_records, "{kind:?}");
    }
}

/// A retired destination is refused with the thing to use instead.
///
/// The variants stay — `SinkKind` is a signed field of a replicated
/// `OpBody::SinkPut`, and postcard encodes an enum by index, so deleting
/// one would re-read every other sink's kind as the wrong destination. What
/// must not survive is the *behaviour*: a record signed before the cut
/// still decodes, and the export loop fails it with a sentence an operator
/// can act on rather than a transport error nobody can.
#[test]
fn a_retired_destination_is_refused_and_names_its_replacement() {
    for kind in [
        SinkKind::Ndjson,
        SinkKind::OpenSearch,
        SinkKind::Postgres,
        SinkKind::SplunkHec,
        SinkKind::Otlp,
    ] {
        let Some(sentence) = kind.retirement() else {
            panic!("{kind:?} is retired but does not say so");
        };
        assert!(
            sentence.contains("has been retired"),
            "{kind:?}: {sentence}"
        );
        // The sentence must not send somebody to a destination that is
        // itself retired — which is exactly what the first of these did
        // until the four connectors went with it.
        for gone in ["opensearch", "postgres", "splunk-hec", "otlp", "ndjson"] {
            assert!(
                !sentence.contains(&format!("`{gone}`"))
                    || sentence.starts_with(&format!("the `{gone}`")),
                "{kind:?} points at the retired `{gone}`: {sentence}"
            );
        }
    }
    // And the live ones are not retired, which is the half that would
    // otherwise rot silently.
    for kind in [
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::Webhook,
        SinkKind::Plugin,
    ] {
        assert!(kind.retirement().is_none(), "{kind:?}");
    }
}
