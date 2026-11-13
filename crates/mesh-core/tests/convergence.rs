//! M5 state hardening (MASTERPLAN §4.4, risk register): merge convergence
//! under adversarial delivery. Anti-entropy may deliver ops in any batch
//! shape and interleaving (per-author order is re-derived from sequence
//! numbers); every honest node must still materialize the identical state.
//!
//! Known, accepted exception (risk register): a *revocation* racing an
//! in-flight op inside the gossip window is order-sensitive by design —
//! revocations are therefore exercised separately, not in the shuffle.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::identity::Keypair;
use mesh_core::state::{MeshState, StateEngine};
use mesh_core::{genesis::Genesis, services};
use mesh_proto::{
    FanId, FanoutControls, FanoutPlanRecord, JobPriority, MemberInfo, OpBody, OpEnvelope, Runtime,
    SignedIpc,
};

/// Tiny deterministic LCG so the shuffle needs no rand dependency and every
/// failure reproduces from its seed.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() as usize) % (i + 1);
            v.swap(i, j);
        }
    }
}

fn member_info(key: &Keypair, name: &str) -> MemberInfo {
    MemberInfo {
        node_id: key.node_id(),
        name: name.into(),
        addrs: vec!["127.0.0.1:7777".into()],
        tags: vec![],
        slots: 8,
        joined_at_micros: mesh_proto::now_micros(),
    }
}

/// A canonical projection of everything M5 rides on, for equality checks.
fn summarize(s: &MeshState) -> String {
    let mut out = String::new();
    for (id, m) in &s.members {
        out.push_str(&format!(
            "member {} {} slots={} tags={:?} left={}\n",
            id.short(),
            m.info.name,
            m.info.slots,
            m.info.tags,
            m.left
        ));
    }
    for (name, e) in &s.services {
        out.push_str(&format!(
            "service {name} head={} versions={:?} removed={}\n",
            e.head,
            e.versions.keys().collect::<Vec<_>>(),
            e.removed
        ));
    }
    for (name, e) in &s.sources {
        out.push_str(&format!(
            "source {name} removed={} last={:?}\n",
            e.removed,
            e.last.as_ref().map(|l| &l.commit)
        ));
    }
    for (group, e) in &s.fanouts {
        out.push_str(&format!(
            "fanout {} owner={} adopters={} closed={:?}\n",
            group.short(),
            e.owner().short(),
            e.adopters.len(),
            e.closed.map(|o| (o.succeeded, o.failed, o.skipped))
        ));
    }
    for (id, j) in &s.jobs {
        out.push_str(&format!("job {} state={:?}\n", id.short(), j.state));
    }
    for (id, u) in &s.users {
        out.push_str(&format!("user {} {}\n", id.short(), u.record.username));
    }
    for (id, g) in &s.grants {
        out.push_str(&format!(
            "grant {} {} revoked={}\n",
            id.to_display_string(),
            g.grant.role.as_str(),
            g.revoked
        ));
    }
    out
}

/// Feed `ops` to `engine` the way hostile gossip might: random batch sizes,
/// random order, duplicates included, over repeated rounds until everything
/// applies (undelivered/parked ops get re-offered like anti-entropy would).
fn deliver_chaotically(engine: &Arc<StateEngine>, ops: &[OpEnvelope], rng: &mut Lcg) {
    for _round in 0..24 {
        let mut shuffled: Vec<OpEnvelope> = ops.to_vec();
        rng.shuffle(&mut shuffled);
        let cut = 1 + (rng.next() as usize) % shuffled.len().max(1);
        shuffled.truncate(cut);
        engine.ingest(shuffled).expect("ingest");
    }
    // Final full delivery so nothing stays parked by unlucky sampling.
    engine.ingest(ops.to_vec()).expect("final ingest");
}

#[test]
fn shuffled_delivery_converges_to_identical_state() {
    let dir_a = tempfile::tempdir().unwrap();
    let founder_user = Keypair::generate().unwrap();
    let founder_node = Keypair::generate().unwrap();
    let genesis = Genesis::create("conv-mesh", &founder_user, founder_node.node_id()).unwrap();
    let a = StateEngine::open(dir_a.path(), founder_node.clone(), genesis.clone()).unwrap();

    // Build a representative history on A: members, a second authoring
    // node, services (user-signed and source-bumped), a git source, a
    // fan-out lifecycle, and job summaries.
    a.author_op(OpBody::MemberAdd {
        member: member_info(&founder_node, "alpha"),
        invite: None,
    })
    .unwrap();
    let node_b = Keypair::generate().unwrap();
    a.author_op(OpBody::MemberAdd {
        member: member_info(&node_b, "beta"),
        invite: None,
    })
    .unwrap();

    let bundle = mesh_proto::BundleMeta {
        bundle_id: mesh_proto::BundleId([9; 32]),
        total_size: 10,
        file_count: 1,
        chunk_count: 1,
    };
    a.author_op(OpBody::BundleAnnounce {
        meta: bundle.clone(),
    })
    .unwrap();
    services::create_service_as(
        &a,
        &founder_user,
        "svc".into(),
        Runtime::Bash,
        bundle.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        String::new(),
        vec![],
    )
    .unwrap();

    // Git source + a source-authorized bump (empty signature path).
    let source = mesh_core::gitsync::build_source(
        &founder_user,
        "src".into(),
        "/tmp/repo".into(),
        "main".into(),
        60,
        vec!["svc".into()],
    );
    a.author_op(OpBody::SourceAdd { source }).unwrap();
    let bundle2 = mesh_proto::BundleMeta {
        bundle_id: mesh_proto::BundleId([10; 32]),
        total_size: 11,
        file_count: 1,
        chunk_count: 1,
    };
    a.author_op(OpBody::BundleAnnounce {
        meta: bundle2.clone(),
    })
    .unwrap();
    a.author_op(OpBody::SourceSynced {
        name: "src".into(),
        commit: "abc123".into(),
        bundle_id: bundle2.bundle_id,
    })
    .unwrap();
    let bumped = mesh_proto::ServiceVersion {
        name: "svc".into(),
        version: 2,
        runtime: Runtime::Bash,
        bundle_id: bundle2.bundle_id,
        working_dir: String::new(),
        entrypoint: "run.sh".into(),
        contract: None,
        env: vec![],
        description: String::new(),
        tags: vec![],
        created_at_micros: mesh_proto::now_micros(),
        created_by: founder_user.user_id(),
        source: Some("src".into()),
        signature: Vec::new(),
    };
    a.author_op(OpBody::ServiceVersionAdd { version: bumped })
        .unwrap();
    a.author_op(OpBody::ServiceHead {
        name: "svc".into(),
        version: 2,
        by: founder_user.user_id(),
        signature: Vec::new(),
    })
    .unwrap();

    // A fan-out plan, adoption, and closure.
    let group = FanId::generate();
    let job = mesh_proto::JobId::generate();
    a.author_op(OpBody::FanoutPlan {
        plan: Box::new(FanoutPlanRecord {
            group,
            payload: mesh_proto::JobPayload::Command("echo hi".into()),
            env: vec![],
            timeout_secs: None,
            priority: JobPriority::Normal,
            submitter: founder_node.node_id(),
            submitter_user: founder_user.user_id(),
            auth: SignedIpc {
                user: founder_user.user_id(),
                at_micros: mesh_proto::now_micros(),
                nonce: 1,
                payload: vec![],
                signature: vec![0; 64],
            },
            targets: vec![mesh_proto::PlannedTarget {
                job_id: job,
                node_id: node_b.node_id(),
                node_name: "beta".into(),
            }],
            controls: FanoutControls::default(),
            created_at_micros: mesh_proto::now_micros(),
        }),
    })
    .unwrap();

    // Beta authors its own ops (job summary + adoption) so the shuffle
    // exercises cross-author interleaving, not just one sequence.
    let dir_b0 = tempfile::tempdir().unwrap();
    let b_engine = StateEngine::open(dir_b0.path(), node_b.clone(), genesis.clone()).unwrap();
    b_engine.ingest(a.all_ops().unwrap()).unwrap();
    b_engine
        .author_op(OpBody::JobUpsert {
            summary: mesh_proto::JobSummary {
                job_id: job,
                cmd_preview: "echo hi".into(),
                runtime: "bash".into(),
                submitter: founder_node.node_id(),
                submitter_user: founder_user.user_id(),
                executor: node_b.node_id(),
                state: mesh_proto::JobState::Succeeded,
                exit_code: Some(0),
                created_at_micros: 1,
                started_at_micros: Some(2),
                finished_at_micros: Some(3),
                fanout: None,
                updated_hlc: b_engine.clock().now(),
            },
        })
        .unwrap();
    b_engine
        .author_op(OpBody::FanoutAdopt {
            group,
            by: node_b.node_id(),
        })
        .unwrap();
    b_engine
        .author_op(OpBody::FanoutClosed {
            group,
            succeeded: 1,
            failed: 0,
            skipped: 0,
        })
        .unwrap();

    // The full multi-author history.
    let mut history = a.all_ops().unwrap();
    let b_vv: Vec<(mesh_proto::NodeId, u64)> = a.version_vector();
    let (beta_ops, _) = b_engine.missing_for(&b_vv).unwrap();
    history.extend(beta_ops);

    // Reference: apply in canonical order on a fresh engine.
    let observer = || {
        let dir = tempfile::tempdir().unwrap();
        let key = Keypair::generate().unwrap();
        let engine = StateEngine::open(dir.path(), key, genesis.clone()).unwrap();
        (dir, engine)
    };
    let (_d_ref, reference) = observer();
    reference.ingest(history.clone()).unwrap();
    let want = reference.read(summarize);
    assert!(want.contains("service svc head=2"), "sanity: {want}");
    assert!(want.contains("adopters=1"), "sanity: {want}");

    // Property: many chaotic delivery schedules, identical final state.
    for seed in 1..=8u64 {
        let (_d, engine) = observer();
        let mut rng = Lcg(seed);
        deliver_chaotically(&engine, &history, &mut rng);
        let got = engine.read(summarize);
        assert_eq!(got, want, "divergence under delivery seed {seed}");
    }
}
