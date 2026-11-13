//! C0 — op-log compaction (C0–C9 §4).
//!
//! The op-log was append-only and never compacted. Every entity in the
//! system up to now has a write rate bounded by human action, so the log
//! grew at human speed and nobody had to care. C1 introduces the first
//! entity whose write rate is proportional to *cluster throughput*, and a
//! log that is replicated to every node and re-sent to every joiner cannot
//! also be a metrics store. Hence this, first.
//!
//! **The discipline is the same one every other subsystem here follows:
//! per-author and local, never coordinated.** A node compacts its own
//! authored log and nobody else's. There is no quorum, no global epoch and
//! nothing to agree on, because the only node that can ever be authoritative
//! about author A's sequence numbers is A.
//!
//! # What compaction actually drops
//!
//! Nothing that carries state. An op is *superseded* when a later op by the
//! same author writes the same thing — the same job's summary, the same
//! host's record, the same subject's facts. The op that won is still in the
//! log, so a joiner that never sees the dropped ones reconstructs exactly
//! the same [`MeshState`](crate::state::MeshState). A checkpoint is a
//! statement about **absence**, which is the one thing a receiver could not
//! otherwise infer: without it, a dropped op is indistinguishable from a
//! lost one and the gapless per-author rule parks everything behind the
//! hole forever.
//!
//! # What never compacts
//!
//! - **The audit log, inside its window.** It is tamper-evidence, so no
//!   later op ever supersedes a row. It leaves only when an admin has
//!   signed a finite `audit` retention window — never shorter than
//!   [`mesh_proto::AUDIT_FLOOR_DAYS`] — and the row is older than that
//!   (ENG-3). Under the shipped `forever` it is [`Compaction::Grow`].
//! - **Constructors.** `MemberAdd` creates the entry `MemberUpdate` patches;
//!   drop the add and the member vanishes. The rule throughout is that a
//!   *total* write may supersede an earlier total write of the same thing,
//!   and a *patch* may only supersede an earlier patch.
//! - **Immutable history.** Service and workflow versions, invite uses,
//!   approval decisions, published releases: each op is a distinct fact,
//!   not a revision of one.
//! - **Tombstones.** A removal that a peer has not merged must not be undone
//!   by an earlier `Put` outliving it.

use std::collections::BTreeMap;

use mesh_proto::{
    Checkpoint, FanId, GangId, NodeId, OpBody, OpEnvelope, RetentionPolicy, SeqRange, seq_ranges,
};

/// Fan-outs and gangs whose records may leave the log with their window
/// (ENG-3), by id, with the instant each closed.
///
/// Only groups that are **closed and were never adopted** belong here, and
/// the caller — the author, reading its own replicated state — decides
/// that. A group nobody else ever wrote about has all of its ops in one
/// author's log, so they leave together and nothing another node holds
/// can be left pointing at a plan that is gone. An adopted group keeps its
/// plan: other authors' adoptions and closes refer to it.
///
/// Not a pure function of the op, unlike every other class here, because
/// a plan does not say when it will close. That is safe for the same
/// reason compaction is per-author: the author alone decides what leaves
/// its log, and everyone else follows its checkpoint.
#[derive(Debug, Clone, Default)]
pub struct RetirableGroups {
    pub fanouts: BTreeMap<FanId, u64>,
    pub gangs: BTreeMap<GangId, u64>,
}

impl RetirableGroups {
    /// The class of a fan-out or gang op whose group may retire, or `None`
    /// to fall back to [`compaction`].
    fn class(&self, body: &OpBody, retention: &RetentionPolicy) -> Option<Compaction> {
        let window = retention.window(mesh_proto::RetentionTable::Runs);
        if window.is_forever() {
            return None;
        }
        let (key, closed_at_micros) = match body {
            OpBody::FanoutPlan { plan } => (
                CompactKey("fanout-plan", plan.group.to_string()),
                *self.fanouts.get(&plan.group)?,
            ),
            OpBody::FanoutClosed { group, .. } => (
                CompactKey("fanout-closed", group.to_string()),
                *self.fanouts.get(group)?,
            ),
            OpBody::GangPlan { plan } => (
                CompactKey("gang-plan", plan.gang.to_string()),
                *self.gangs.get(&plan.gang)?,
            ),
            OpBody::GangClosed { gang, .. } => (
                CompactKey("gang-closed", gang.to_string()),
                *self.gangs.get(gang)?,
            ),
            _ => return None,
        };
        Some(Compaction::Closed {
            key,
            closed_at_micros,
            expires_micros: closed_at_micros
                .saturating_add(u64::from(window.days) * 86_400 * 1_000_000),
        })
    }
}

/// How one op relates to later ops by the same author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compaction {
    /// Grow-only: this op is never dead weight, whatever follows it.
    Grow,
    /// Last-writer-wins over this key. An earlier op by the same author
    /// with an equal key is superseded and eligible to be dropped.
    Lww(CompactKey),
    /// Last-writer-wins over this key **and** droppable outright once
    /// `expires_micros` has passed — the survivor included (C0 P3).
    ///
    /// This is the shape a decay window needs and plain LWW cannot express.
    /// A usage counter for last February is nobody's dead weight: it is the
    /// current, winning value for its key, and LWW would keep it for ever.
    /// What makes it droppable is not that something superseded it but that
    /// **nothing reads it any more**.
    ///
    /// That last clause is the safety argument, and it is the only one.
    /// Dropping a live value would diverge two nodes; dropping a value that
    /// every reader already filters out cannot, because the filter is a
    /// function of the clock and the mesh-wide constant
    /// [`mesh_proto::USAGE_DECAY_SECS`] — never of who happened to compact.
    /// A node that never compacts and a node that compacts hourly answer
    /// every question identically. Compaction only stops one of them paying
    /// to store an answer neither would give.
    Decaying {
        key: CompactKey,
        expires_micros: u64,
    },
    /// Last-writer-wins over this key **and** droppable outright once the
    /// entity has been *closed* for longer than the mesh's retention window
    /// (M31 A1).
    ///
    /// The fourth class, and it exists because C0 closed only half of the
    /// problem. C0 drops what a later op **supersedes**. A finished job's
    /// last `JobUpsert` supersedes nothing and is superseded by nothing, so
    /// it survives for ever — correctly, under the rules as written. One op
    /// per job, on every node, permanently, is the wall M31 §1 measures.
    ///
    /// So the eligibility rule is a different sentence: *an op that is
    /// closed is droppable not because something replaced it, but because
    /// it is old enough that nobody is asking any more.*
    ///
    /// **How this differs from [`Decaying`], since the shapes are twins.**
    /// A decayed usage window is droppable because *nothing reads it* — the
    /// filter is a function of the clock and a mesh-wide **constant**, so
    /// two nodes cannot disagree and dropping is invisible. A closed record
    /// is different: it is read right up until the window passes, and the
    /// window is **policy**. That is exactly why the horizon is replicated
    /// (C9) and why the readers filter on the same policy the compactor
    /// does. Two nodes with different node config and the same replicated
    /// policy must drop the same ops and answer the same questions, and
    /// that pair of properties is the whole safety argument for this class.
    ///
    /// [`Decaying`]: Self::Decaying
    Closed {
        key: CompactKey,
        /// When the entity reached its terminal state. `expires` is this
        /// plus the replicated window, computed by the caller that holds
        /// the policy — the classification stays a pure function of the op.
        closed_at_micros: u64,
        expires_micros: u64,
    },
}

/// The identity of the thing an op writes: a stable kind tag and the
/// entity's key within it. Two ops collide only if both halves match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompactKey(pub &'static str, pub String);

/// Classify an op body for compaction, under a retention policy.
///
/// This is the whole of the `Compactable` decision, in one exhaustive
/// match, deliberately: adding an entity kind to `OpBody` without deciding
/// whether it compacts should not compile. **When in doubt the answer is
/// [`Compaction::Grow`]** — a log that is larger than it needs to be is a
/// cost, and a log that dropped something load-bearing is a corruption.
///
/// `retention` is the **replicated** policy, never node config (C9). It
/// enters here rather than at the call site because [`Compaction::Closed`]
/// carries an absolute expiry, exactly as [`Compaction::Decaying`] does —
/// which keeps [`plan`] a single loop that does not have to know which
/// classes are time-based.
pub fn compaction(body: &OpBody, retention: &RetentionPolicy) -> Compaction {
    use Compaction::Grow;
    fn key(kind: &'static str, id: impl Into<String>) -> Compaction {
        Compaction::Lww(CompactKey(kind, id.into()))
    }
    /// A terminal record's key, plus the instant its window closes.
    ///
    /// Falls back to plain LWW when the entity is **not terminal** or has no
    /// finish instant. That fallback is the load-bearing half: a job still
    /// running must never be dropped whatever its age, and the ordinary rule
    /// that a superseded write is dead weight still applies to it. The key
    /// is the same either way, so the queued/running/finished ops of one job
    /// still supersede each other across the transition.
    fn closed(
        kind: &'static str,
        id: impl Into<String>,
        terminal: bool,
        finished_at_micros: Option<u64>,
        window: mesh_proto::RetentionWindow,
    ) -> Compaction {
        let k = CompactKey(kind, id.into());
        match (
            terminal,
            finished_at_micros.filter(|f| *f > 0),
            window.is_forever(),
        ) {
            (true, Some(closed_at_micros), false) => Compaction::Closed {
                key: k,
                closed_at_micros,
                expires_micros: closed_at_micros
                    .saturating_add(u64::from(window.days) * 86_400 * 1_000_000),
            },
            _ => Compaction::Lww(k),
        }
    }
    let runs_window = retention.window(mesh_proto::RetentionTable::Runs);
    match body {
        // -- membership -----------------------------------------------------
        // `MemberAdd` is the constructor and `MemberLeft`/`MemberRevoke` are
        // tombstones; only the attribute patch collapses, and it is total
        // over every field it touches.
        OpBody::MemberAdd { .. } | OpBody::MemberLeft { .. } | OpBody::MemberRevoke { .. } => Grow,
        OpBody::MemberUpdate { node_id, .. } => key("member-attrs", node_id.to_string()),
        // One standing decision per node: an admin who changes their mind
        // twice has said one thing, and only the latest of it is a fact.
        OpBody::NodeExecutionSet { decision } => {
            key("node-execution", decision.node_id.to_string())
        }
        // One standing decision per node, same as above — but note this
        // one is *security* history, so the audit entries beside it (which
        // grow, always) are what an incident review reads. The state here
        // is only "is it isolated right now".
        OpBody::NodeQuarantineSet { quarantine } => {
            key("node-quarantine", quarantine.node_id.to_string())
        }
        // One assigned tag list per node, total over the whole list — so
        // an operator who has relabelled a machine six times leaves one op
        // behind, not six.
        OpBody::NodeTagsSet { assignment } => key("node-tags", assignment.node_id.to_string()),

        // -- invites --------------------------------------------------------
        // A use is an OR-set element, not a revision, and the public record
        // and its revocation are one write each.
        OpBody::InviteCreate { .. }
        | OpBody::InviteConsume { .. }
        | OpBody::InviteRevoke { .. } => Grow,

        // -- jobs -----------------------------------------------------------
        // The first op in the system with a machine write rate: queued,
        // running, finished, one upsert each, of which only the last is the
        // job. This is the single largest win in the pass.
        // M31 A1: a job that has *finished* leaves the replicated log once
        // the mesh's run window has passed. One that has not is LWW as it
        // always was, and is never dropped whatever its age — the
        // `sweep_retention` rule (`jobs.rs`), restated on the replicated
        // side.
        OpBody::JobUpsert { summary } => closed(
            "job",
            summary.job_id.to_string(),
            summary.state.is_terminal(),
            summary.finished_at_micros,
            runs_window,
        ),
        // One clear supersedes every earlier one from the same admin: the
        // watermark merges by max, so only the latest carries information.
        //
        // What this deliberately does *not* do is collapse the `JobUpsert`
        // ops a clear made inert. They are another author's log and this
        // pass only ever rewrites our own — so a clear frees the state map
        // and this node's job records, not op-log bytes. Those go on the
        // ordinary schedule, one surviving op per job, as they always did.
        OpBody::JobsClear { .. } => key("jobs-clear", String::new()),
        // Forcing the same job twice carries one fact; the later op's HLC
        // is the one the map keeps, so the earlier is inert.
        OpBody::JobCancelForce { job_id, .. } => key("job-cancel-force", job_id.to_string()),

        // -- content --------------------------------------------------------
        OpBody::BundleAnnounce { meta } => key("bundle", meta.bundle_id.to_string()),
        // Re-announcing a hold this node already holds is idempotent; the
        // *set* is grow-only across authors, and within one author the
        // latest re-announcement is the whole of its contribution.
        OpBody::BundleHold { bundle_id, node_id } => {
            key("bundle-hold", format!("{bundle_id}:{node_id}"))
        }
        // Versions are immutable facts; removal is a tombstone.
        OpBody::ServiceVersionAdd { .. } | OpBody::ServiceRemove { .. } => Grow,
        OpBody::ServiceHead { name, .. } => key("service-head", name.clone()),

        // -- governance -----------------------------------------------------
        // Identity and authority are grow-only on purpose: every one of
        // these is evidence of who was allowed to do what, and when.
        OpBody::UserAdd { .. }
        | OpBody::UserRevoke { .. }
        // A machine principal is identity, so it grows like the rest of it.
        // First-add-wins on merge means there is never a second one to
        // supersede anyway — and if there were, dropping the earlier would
        // erase the record of what a pipeline's credential was originally
        // scoped to, which is the question an incident asks first.
        | OpBody::ServiceAccountAdd { .. }
        | OpBody::GrantAdd { .. }
        | OpBody::GrantRevoke { .. }
        | OpBody::SecretPut { .. }
        | OpBody::SecretRemove { .. } => Grow,
        // A rewrap *in the current form* is its author's total restatement
        // of the wraps it provides for one version, so a later one
        // supersedes it (SEC-17 made that true).
        //
        // **An older node's rewrap is not, and must not be collapsed**
        // (ENG-18). Before SEC-17 each one carried only the recipients that
        // were missing at the time, and the apply path still adds those to
        // what it has rather than replacing it — because that is the only
        // reading of them that is correct. Compacting them by the same LWW
        // key threw away every recipient but the last op's: the author
        // itself would not notice, since its live state was built before
        // the drop, but a restart, a new joiner or a node rebuilt from
        // backup replays what is left and ends up unable to open a secret
        // it is entitled to. A log this node has already upgraded past can
        // still hold them, so the distinction has to be drawn here and not
        // assumed away.
        OpBody::SecretRewrap {
            name,
            version,
            wraps,
        } => {
            if crate::secrets::is_total_statement(wraps) {
                key("secret-rewrap", format!("{name}@{version}"))
            } else {
                Grow
            }
        }
        // -- identity provider (E1-P5) --------------------------------------
        // One configuration per provider name: an admin who reconfigures
        // has said one thing. The removal is a tombstone and grows.
        OpBody::OidcProviderPut { provider } => key("oidc-provider", provider.name.clone()),
        OpBody::OidcProviderRemove { .. } => Grow,
        // One current assertion per key. An older verification is genuinely
        // superseded — it described the same person at an earlier moment,
        // and the approval decision is made against the latest.
        OpBody::OidcAssertionPut { assertion } => {
            key("oidc-assertion", assertion.user_id.to_string())
        }

        // Tamper-evidence: nothing supersedes a row. Under a finite audit
        // window (never below the floor) a row leaves once it is older than
        // the window, like a closed job (ENG-3); under `forever` it stays.
        OpBody::AuditAppend { entry } => {
            let window = retention.window(mesh_proto::RetentionTable::Audit);
            if window.is_forever() {
                Grow
            } else {
                closed(
                    "audit",
                    entry.audit_id.to_string(),
                    true,
                    Some(entry.at_micros),
                    window,
                )
            }
        }
        OpBody::EnrolRequest { request } => key("enrolment", request.user_id.to_string()),
        OpBody::EnrolDecline { .. } => Grow,

        // -- fan-out and workflow runs --------------------------------------
        // Plans are constructors — a successor adopting a dead origin's
        // fan-out needs the payload — so they stay. Adoption claims and
        // closures are LWW per group, and the run record is a job summary
        // by another name.
        OpBody::FanoutPlan { .. } => Grow,
        OpBody::FanoutAdopt { group, .. } => key("fanout-adopt", group.to_string()),
        OpBody::FanoutClosed { group, .. } => key("fanout-closed", group.to_string()),

        // -- sources --------------------------------------------------------
        OpBody::SourceAdd { source } => key("source", source.name.clone()),
        OpBody::SourceRemove { .. } => Grow,
        // One per poll interval, forever, per source. A machine write rate
        // that predates C1 and has been quietly filling logs since M5.
        OpBody::SourceSynced { name, .. } => key("source-synced", name.clone()),

        // Attesting a key is a fact about an identity and the evidence for
        // it; a revocation is the record of a decision. Collapsing an
        // attestation into a later revocation would lose *when* a key was
        // believed non-exportable, which is the question an auditor asks
        // about a withdrawn one (Y2/Y3).
        OpBody::UserHardwareAttest { .. } | OpBody::UserHardwareRevoke { .. } => Grow,
        // One standing decision per authority, total over the record —
        // `ReleaseKeyAdd`'s shape, because it is the same kind of trust.
        // One co-signature per binary, total over the record: an admin who
        // re-published one has published the same fact again (Y6).
        OpBody::ReleaseCosign { cosign } => key("release-cosign", cosign.key()),
        OpBody::AttestRootAdd { root } => key("attest-root", root.id()),
        OpBody::AttestRootRemove { .. } => Grow,


        OpBody::FactCollectorPut { collector } => key("collector", collector.name.clone()),
        // The other pre-C1 machine write rate: every node re-publishes
        // every document on every collection interval, and only the newest
        // is ever read.
        //
        // **LWW alone does not bound it** (ENG-19). A subject that stops
        // being collected — a host taken out of the inventory, a collector
        // an admin removed, a machine that left — leaves its last document
        // in the log for the life of the mesh, because nothing supersedes
        // it and nothing expires it. That is what the `facts` window is
        // for, and it was the one window with no enforcement path at all.
        //
        // Decaying rather than Closed, for the reason that class documents:
        // what makes the survivor droppable is not that something replaced
        // it but that *nothing reads it any more*. The cutoff is the
        // replicated policy and the collection instant, so every node —
        // one that compacts hourly and one that never has — answers
        // identically. `StateEngine::expire_closed` drops the same
        // documents from memory on the same cutoff, so a long-running node
        // and a freshly replayed one agree.
        //
        // A live fact is never at risk: it is republished every collection
        // interval, and the window that would have to elapse between two of
        // those is a year by default.
        OpBody::FactsPublish {
            subject,
            collector,
            collected_at_micros,
            ..
        } => {
            let k = CompactKey("facts", format!("{}/{collector}", subject.label()));
            match retention
                .window(mesh_proto::RetentionTable::Facts)
                .span_micros()
            {
                // `forever`: an operator typed it.
                None => Compaction::Lww(k),
                Some(span) => Compaction::Decaying {
                    key: k,
                    expires_micros: collected_at_micros.saturating_add(span),
                },
            }
        }

        // -- sessions -------------------------------------------------------
        OpBody::SessionPolicyPut { policy } => key("session-policy", policy.id.clone()),
        OpBody::SessionPolicyRemove { .. } => Grow,

        // -- fleet upgrade --------------------------------------------------
        OpBody::UpgradePlanPut { plan } | OpBody::UpgradePlanForce { plan } => {
            key("upgrade-plan", plan.plan_id.to_string())
        }
        OpBody::UpgradePlanCancel { .. } => Grow,
        OpBody::ReleasePublish { .. } => Grow,
        OpBody::ReleaseKeyPut { key: k } => key("release-key", k.pubkey.clone()),
        OpBody::ReleaseKeyRevoke { .. } => Grow,


        // -- usage counters (C0 P3) -----------------------------------------
        // Two rules at once, and it needs both. Within a live window a node
        // republishes its running total as work finishes, and only the last
        // one is the counter — that is the LWW half, and it is what keeps a
        // busy hour from costing a thousand ops. Past the decay horizon the
        // survivor goes too, because a window nobody reads is not state.
        OpBody::UsageAdd { usage } => Compaction::Decaying {
            key: CompactKey("usage", format!("{}/{}", usage.principal, usage.window)),
            expires_micros: mesh_proto::usage_window_end_micros(usage.window)
                .saturating_add(mesh_proto::USAGE_DECAY_SECS.saturating_mul(1_000_000)),
        },


        // -- deployments (C2 §6) --------------------------------------------
        // A deployment is LWW per name exactly as a schedule is: a pause, a
        // rollout and a rollback each supersede the last put. The tombstone
        // grows. The per-node health aggregate collapses to one live op per
        // node, the reservations rule again.
        OpBody::DeploymentPut { deployment } => key("deployment", deployment.name.clone()),
        // The policy beside it, same rule: an operator who has changed the
        // canary count six times has said one thing (N1 P3/P4).
        OpBody::DeploymentPolicyPut { policy } => {
            key("deployment-policy", policy.deployment.clone())
        }

        // -- service schedules (N1 P5) --------------------------------------
        // LWW per name, exactly as a deployment is: a pause, an edit and a
        // retarget each supersede the last put. The tombstone grows.
        OpBody::ServiceSchedulePut { schedule } => {
            key("service-schedule", schedule.name.clone())
        }
        OpBody::ServiceScheduleRemove { .. } => Grow,

        // -- the service registry (N3 P1) -----------------------------------
        // One live op per node, the `ReservationsPut` rule a third time: a
        // node that has republished its registrations four hundred times
        // has said one thing, and only the latest of it is a fact.
        OpBody::RegistryPut { .. } => key("registry", String::new()),

        // -- the mesh-wide shed (N6 §12) ------------------------------------
        // One live op for the whole mesh, and an empty key is exactly
        // right: there is one of these, ever, and the tenth flip supersedes
        // the ninth. A shed history is what the audit log is for.
        OpBody::ShedPut { .. } => key("shed", String::new()),
        // One live record per deployment each (Phase B, E2): the tenth
        // `auto_promote` flip has said one thing, and only the latest
        // promotion can name the generation rolling now.
        OpBody::DeploymentKnobsPut { knobs } => key("deployment-knobs", knobs.deployment.clone()),
        OpBody::DeploymentPromote { promotion } => {
            key("deployment-promote", promotion.deployment.clone())
        }
        // A scope is ordered by its signed time, not by where it sits in its
        // author's log, so an earlier op can still be the one that wins:
        // kept, all of them. They are an admin's rare writes.
        OpBody::SecretScopeSet { .. } => Grow,
        // Terminal, and the log it sits in is moved aside moments after it
        // lands. Nothing supersedes it.
        OpBody::MeshReset { .. } => Grow,

        // -- deployment history (Phase B, E1) -------------------------------
        // Per generation, on a ring twice the size of what state shows: a
        // version is superseded by a later publish of the *same* generation
        // (a scale, a pause) or by one twenty generations on, never by the
        // next roll. Twice, so the newest ten survive in every author's log
        // however the publishes were spread across daemons. A tag is LWW per
        // name, and removing one is the write that supersedes it.
        OpBody::DeploymentVersionPut { version } => key(
            "deployment-version",
            format!(
                "{}\u{0}{}",
                version.deployment.name,
                version.deployment.generation % (mesh_proto::DEPLOYMENT_HISTORY_KEEP as u32 * 2)
            ),
        ),
        OpBody::DeploymentTagPut { tag } => {
            key("deployment-tag", format!("{}\u{0}{}", tag.deployment, tag.tag))
        }
        OpBody::DeploymentRemove { .. } => Grow,
        OpBody::DeploymentHealthPut { .. } => key("deployment-health", String::new()),

        // -- gangs (C4 §8) --------------------------------------------------
        // The fan-out rules verbatim, because the entities are the same
        // shape: an immutable plan grows, and the adoption claim and the
        // terminal record each collapse to one live op per gang.
        OpBody::GangPlan { .. } => Grow,
        OpBody::GangAdopt { gang, .. } => key("gang-adopt", gang.to_string()),
        OpBody::GangClosed { gang, .. } => key("gang-closed", gang.to_string()),

        // -- devices (C5 §9) ------------------------------------------------
        // The reservations rule a third time: a node republishes its whole
        // inventory whenever the hardware or a device's health changes, and
        // only the newest is what the node has. Collapses to exactly one
        // live op per node however many driver upgrades it has seen.
        OpBody::DeviceInventoryPut { .. } => key("device-inventory", String::new()),

        // -- retention (M31 A1) ---------------------------------------------
        // One record for the whole mesh, LWW like every other configured
        // entity. A reset is the `JobsClear` shape: only the newest carries
        // information, and it keys separately from the put so neither
        // supersedes the other — the HLC decides which one state applies.
        OpBody::RetentionPut { .. } => key("retention", String::new()),
        OpBody::RetentionReset { .. } => key("retention-reset", String::new()),

        // -- sinks (M31 B1) ---------------------------------------------------
        // LWW per name, like every other configured entity, and a tombstone
        // that grows. Classified in the same commit that adds the variants,
        // which is this exhaustive match doing its job: C0 would refuse to
        // compile otherwise.
        OpBody::SinkPut { sink } => key("sink", sink.name.clone()),
        OpBody::SinkRemove { .. } => Grow,
        // The `ReservationsPut` rule: one live op per sink per author,
        // however long the mesh runs. A gate that grew the log it gates
        // would be self-defeating.
        OpBody::SinkProgress { sink, .. } => key("sink-progress", sink.clone()),

        // -- singleton claims -----------------------------------------------
        // One live claim per name: the last submission under a name is the
        // only one whose job can still be running.
        OpBody::SingletonClaim { name, .. } => key("singleton", name.clone()),

        // -- the checkpoint itself ------------------------------------------
        // Cumulative by construction (see `plan`), so the newest one tells
        // the whole story and every earlier one is dead weight. If this were
        // `Grow`, compaction would leak one op per pass forever — which is
        // the bug every log-compaction implementation writes once.
        OpBody::Checkpoint { .. } => key("checkpoint", String::new()),
    }
}

/// What a compaction pass would do, computed without touching the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Sequence numbers **this pass** newly finds droppable — superseded,
    /// older than the horizon — sorted ascending.
    ///
    /// Newly, not cumulatively (**M-11**). What earlier passes already took
    /// is carried in [`Self::already`] as *ranges*: expanding them back
    /// into individual numbers so the two could share this `Vec` meant a
    /// mesh that had pruned five million ops rebuilt and re-sorted a 40 MB
    /// list every hour, to rediscover a set it had just been handed in
    /// run-length form.
    pub drop: Vec<u64>,
    /// What previous passes already dropped, as ranges — never expanded.
    pub already: Vec<SeqRange>,
    /// Ops the author holds today.
    pub ops_before: u64,
    /// How many were superseded but *retained* because they are still
    /// inside the horizon. The number that says "come back later".
    pub held_by_horizon: u64,
    /// Dropped because their decay window is past the horizon anyone reads
    /// — the counters C1 will write, aged out (C0 P3).
    pub decayed: u64,
    /// Dropped because the entity closed longer ago than the mesh's
    /// retention window (M31 A1). The number that answers *"is the log
    /// actually bounded now"*, and the one an operator watches after
    /// shortening `runs`.
    pub retired: u64,
    /// Past the window, but held back because a sink has not exported them
    /// yet and its ceiling has not passed (M31 B2, §19 Q3).
    ///
    /// Separate from `held_by_horizon` because the two waits have different
    /// fixes: one resolves itself with time, and one means somebody's
    /// warehouse is down and an operator should go and look.
    pub held_for_export: u64,
    /// Dropped **despite** a sink not having exported them, because the
    /// gate's ceiling passed (§18 decision 03).
    ///
    /// The number that must be said out loud. Every one of these is a row
    /// that will never reach the destination, and an operator finding that
    /// out from a query six months later is the outcome this counter
    /// exists to prevent.
    pub forced_gaps: u64,
}

impl CompactionPlan {
    pub fn is_empty(&self) -> bool {
        self.drop.is_empty() && self.already.is_empty()
    }

    /// Everything this plan says is gone, new and previously-pruned alike,
    /// merged and normalised — a two-pointer merge of two sorted range
    /// lists rather than a sort of every sequence number (**M-11**).
    pub fn ranges(&self) -> Vec<SeqRange> {
        let mut all = seq_ranges(&self.drop);
        all.extend(self.already.iter().copied());
        all.sort_unstable();
        let mut merged: Vec<SeqRange> = Vec::with_capacity(all.len());
        for r in all {
            match merged.last_mut() {
                Some(last) if r.lo <= last.hi.saturating_add(1) => last.hi = last.hi.max(r.hi),
                _ => merged.push(r),
            }
        }
        merged
    }

    /// How many sequence numbers the plan covers in total.
    pub fn dropped_total(&self) -> u64 {
        self.ranges()
            .iter()
            .map(|r| r.hi.saturating_sub(r.lo).saturating_add(1))
            .sum()
    }

    /// Is `seq` already accounted for, new or previously pruned?
    pub fn covers(&self, seq: u64) -> bool {
        self.drop.binary_search(&seq).is_ok() || self.already.iter().any(|r| r.contains(seq))
    }
}

/// Decide what to drop from one author's own ops.
///
/// `ops` must be that author's ops in ascending `seq` order. `horizon_micros`
/// is the wall-clock cutoff: an op whose HLC is at or after it is retained
/// even when superseded, because the horizon is what makes a compacted log
/// still useful for answering "what changed last week".
///
/// `already_dropped` are seqs a previous pass removed; they are folded into
/// the result so that every checkpoint this author writes is **cumulative**
/// — one op that tells a joiner the whole story of what is missing.
/// The retention gate: how long a closed record is held back because a sink
/// has not exported it yet, and the point past which it is dropped anyway
/// (M31 B2, §19 Q3; §18 decision 03).
///
/// Computed once per pass by the caller, from replicated state, and passed
/// in — so `plan` stays a pure function and the gate cannot be resolved
/// differently on two nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportGate {
    /// The oldest finish instant that any gating sink has **not** yet
    /// exported. Records at or below it are safely elsewhere.
    pub exported_through_micros: u64,
    /// How far past the retention window a record may be held. Zero — the
    /// default — is no gate at all.
    pub hold_micros: u64,
}

impl ExportGate {
    /// Should this record be held back?
    ///
    /// Three conditions, and the third is the one §18 decision 03 insists
    /// on: a gate is only a gate up to its ceiling. Past that meshware
    /// keeps deleting and reports the gap as permanent, because a stalled
    /// sink must never pin the op-log open on every node in the fleet for
    /// ever.
    fn holds(&self, closed_at_micros: u64, expires_micros: u64, now_micros: u64) -> bool {
        self.hold_micros > 0
            && closed_at_micros > self.exported_through_micros
            && now_micros < expires_micros.saturating_add(self.hold_micros)
    }
}

pub fn plan(
    ops: &[OpEnvelope],
    now_micros: u64,
    horizon_secs: u64,
    already_dropped: &[SeqRange],
    retention: &RetentionPolicy,
    gate: &ExportGate,
) -> CompactionPlan {
    plan_with(
        ops,
        now_micros,
        horizon_secs,
        already_dropped,
        retention,
        gate,
        &RetirableGroups::default(),
    )
}

/// [`plan`], with the fan-outs and gangs the author has found retirable
/// ([`RetirableGroups`]).
pub fn plan_with(
    ops: &[OpEnvelope],
    now_micros: u64,
    horizon_secs: u64,
    already_dropped: &[SeqRange],
    retention: &RetentionPolicy,
    gate: &ExportGate,
    groups: &RetirableGroups,
) -> CompactionPlan {
    let horizon_micros = horizon_cutoff_at(now_micros, horizon_secs);
    // Walk backwards: the first time a key is seen is the op that won.
    let mut seen: BTreeMap<CompactKey, ()> = BTreeMap::new();
    let mut drop: Vec<u64> = Vec::new();
    let mut held = 0u64;
    let mut decayed = 0u64;
    let mut retired = 0u64;
    let mut held_for_export = 0u64;
    let mut forced_gaps = 0u64;

    for op in ops.iter().rev() {
        let Some(body) = op.decode_body() else {
            // An op this build cannot decode is carried, never classified.
            // Guessing at the compaction class of a body from a newer
            // protocol is exactly how a node deletes something it does not
            // understand (F0 §3.2).
            continue;
        };
        let class = groups
            .class(&body, retention)
            .unwrap_or_else(|| compaction(&body, retention));
        let (k, expired, closed) = match class {
            Compaction::Grow => continue,
            Compaction::Lww(k) => (k, false, false),
            Compaction::Decaying {
                key,
                expires_micros,
            } => (key, expires_micros <= now_micros, false),
            // M31 A1. The survivor goes once the *replicated* window has
            // passed, and the reader-side filter uses the same policy — so
            // a node that never compacts and a node that compacts hourly
            // still answer every question identically.
            Compaction::Closed {
                key,
                closed_at_micros,
                expires_micros,
            } => {
                // §19 Q3. Held only while a gating sink has not taken it
                // *and* the ceiling has not passed. `held_for_export` is
                // counted separately from `held_by_horizon` because the two
                // are different waits with different fixes — one resolves
                // itself, and one means somebody's warehouse is down.
                if gate.holds(closed_at_micros, expires_micros, now_micros) {
                    held_for_export += 1;
                    // **Claim the key on the way out.** A held record is
                    // still the current value for its entity, so the
                    // queued/running writes behind it are still dead
                    // weight — and a `continue` that skipped this would
                    // resurrect them for as long as the gate held. The
                    // census had the same bug in the same shape; see
                    // `a_superseded_write_is_never_counted_as_permanent`.
                    seen.insert(key, ());
                    continue;
                }
                let expired = expires_micros <= now_micros;
                // Past the ceiling with the sink still behind: the record
                // goes, and the gap is permanent. Counted so it can be said
                // out loud rather than discovered in a warehouse six months
                // later.
                if expired
                    && gate.hold_micros > 0
                    && closed_at_micros > gate.exported_through_micros
                {
                    forced_gaps += 1;
                }
                (key, expired, true)
            }
        };
        let superseded = seen.insert(k, ()).is_some();
        if expired {
            // Past the decay horizon the survivor goes too — and without
            // waiting on the compaction horizon, which answers a different
            // question. Nothing reads this, so there is nothing to keep it
            // for.
            if closed {
                retired += 1;
            } else {
                decayed += 1;
            }
            drop.push(op.seq);
            continue;
        }
        if !superseded {
            continue; // the current value: never dropped
        }
        if op.hlc.wall_micros >= horizon_micros {
            held += 1;
            continue;
        }
        drop.push(op.seq);
    }

    drop.sort_unstable();
    drop.dedup();

    CompactionPlan {
        drop,
        already: already_dropped.to_vec(),
        ops_before: ops.len() as u64,
        held_by_horizon: held,
        decayed,
        retired,
        held_for_export,
        forced_gaps,
    }
}

/// Build the checkpoint op body for a plan.
pub fn checkpoint_body(
    plan: &CompactionPlan,
    through_seq: u64,
    horizon_secs: u64,
    ops_after: u64,
) -> OpBody {
    OpBody::Checkpoint {
        checkpoint: Box::new(Checkpoint {
            through_seq,
            subsumed: plan.ranges(),
            ops_before: plan.ops_before,
            ops_after,
            horizon_secs,
        }),
    }
}

/// The wall-clock cutoff for a horizon, given "now".
pub fn horizon_cutoff_at(now_micros: u64, horizon_secs: u64) -> u64 {
    now_micros.saturating_sub(horizon_secs.saturating_mul(1_000_000))
}

/// A set of an author's known-absent sequence numbers, kept sorted and
/// coalesced. The merge side holds one per author.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunedSet {
    ranges: Vec<SeqRange>,
    /// Where the newest checkpoint that vouched for these ranges sits in
    /// the author's log. Kept so the sync path can find and serve it: a
    /// peer that is behind on a compacted author needs the checkpoint *in
    /// the same batch* as the ops around the hole, or it parks on the hole
    /// and the batch repeats forever.
    checkpoint_seq: u64,
}

impl PrunedSet {
    pub fn from_ranges(ranges: &[SeqRange]) -> PrunedSet {
        let mut set = PrunedSet::default();
        set.insert_all(ranges);
        set
    }

    /// The seq of the newest checkpoint seen from this author, or 0.
    pub fn checkpoint_seq(&self) -> u64 {
        self.checkpoint_seq
    }

    /// Note the checkpoint that carried these ranges. Monotonic: a stale
    /// re-delivery of an older checkpoint must not move the pointer back.
    pub fn note_checkpoint_at(&mut self, seq: u64) {
        self.checkpoint_seq = self.checkpoint_seq.max(seq);
    }

    pub fn insert_all(&mut self, ranges: &[SeqRange]) -> bool {
        let before = self.ranges.clone();
        self.ranges.extend(ranges.iter().copied());
        self.ranges.sort_unstable();
        let mut merged: Vec<SeqRange> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if r.lo <= last.hi.saturating_add(1) => last.hi = last.hi.max(r.hi),
                _ => merged.push(r),
            }
        }
        self.ranges = merged;
        self.ranges != before
    }

    pub fn contains(&self, seq: u64) -> bool {
        self.ranges
            .binary_search_by(|r| {
                if r.hi < seq {
                    std::cmp::Ordering::Less
                } else if r.lo > seq {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    pub fn ranges(&self) -> &[SeqRange] {
        &self.ranges
    }

    pub fn count(&self) -> u64 {
        self.ranges.iter().map(|r| r.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Advance a gapless high-water mark past every seq known to be absent.
    ///
    /// This is the whole of the merge-side contract: `applied` stays a
    /// scalar, and a hole the author has vouched for is treated as
    /// delivered. A hole nobody has vouched for still parks, which is what
    /// keeps a genuinely lost op from being silently skipped.
    pub fn advance(&self, mut applied: u64) -> u64 {
        while self.contains(applied + 1) {
            applied += 1;
        }
        applied
    }
}

/// A node's op-log figures, for `meshnode state show`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpStats {
    pub ops_total: u64,
    pub ops_mine: u64,
    pub pruned_mine: u64,
    /// What a pass would drop right now, at the configured horizon.
    pub droppable_now: u64,
    /// Superseded but inside the horizon.
    pub held_by_horizon: u64,
    pub horizon_secs: u64,
    /// Usage windows still read, and windows in state that have aged out.
    pub usage_live: u64,
    pub usage_expired: u64,
    /// Sum of pruned seqs across every author, this node included.
    pub pruned_mesh: u64,
    pub bytes_on_disk: u64,
}

/// Per-author pruned sets, keyed by author.
pub type PrunedMap = BTreeMap<NodeId, PrunedSet>;

// ---------------------------------------------------------------------------
// The census (M31 P0)
// ---------------------------------------------------------------------------

/// How far back "recent" reaches when measuring the growth rate.
///
/// A day, because that is the unit the question is asked in — *"how much
/// does this log grow per day"* — and because a shorter window on a mesh
/// with an overnight batch schedule measures the quiet hours.
pub const CENSUS_WINDOW_MICROS: u64 = 24 * 3_600 * 1_000_000;

pub use mesh_proto::{OpCensus, OpKindCount};

/// Count a log by entity, and work out how much of it is permanent.
///
/// `ops` may span every author — unlike [`plan`], which is strictly this
/// node's own log, because compaction is per-author and measurement is not.
/// Supersession is still resolved **per author**, since an op is only ever
/// superseded by a later op from the same author.
///
/// `retention` is the replicated policy, and it changes the answer: under a
/// finite `runs` window a closed job's surviving op is **not** permanent, so
/// it stops counting towards the wall. That is the point — the census is
/// what an operator reads to see whether A1 actually bounded the thing, and
/// it has to move when the policy does.
pub fn census(
    ops: &[OpEnvelope],
    now_micros: u64,
    disk_bytes: u64,
    retention: &RetentionPolicy,
) -> OpCensus {
    census_with(
        ops,
        now_micros,
        disk_bytes,
        retention,
        &RetirableGroups::default(),
    )
}

/// [`census`], with the groups this node knows to be retirable — its own
/// and everyone else's, since the census counts every author.
pub fn census_with(
    ops: &[OpEnvelope],
    now_micros: u64,
    disk_bytes: u64,
    retention: &RetentionPolicy,
    groups: &RetirableGroups,
) -> OpCensus {
    let recent_from = now_micros.saturating_sub(CENSUS_WINDOW_MICROS);
    let mut totals: BTreeMap<&'static str, OpKindCount> = BTreeMap::new();
    let mut out = OpCensus {
        disk_bytes,
        ..Default::default()
    };

    // Per author, newest first: the first time a compaction key is seen is
    // the op that won. An op with no key — `Grow` — always survives.
    let mut by_author: BTreeMap<NodeId, Vec<&OpEnvelope>> = BTreeMap::new();
    for op in ops {
        by_author.entry(op.author).or_default().push(op);
    }

    for author_ops in by_author.values_mut() {
        author_ops.sort_by_key(|o| o.seq);
        let mut seen: BTreeMap<CompactKey, ()> = BTreeMap::new();
        for op in author_ops.iter().rev() {
            let bytes = (op.body.len() + op.signature.len()) as u64;
            let wall = op.hlc.wall_micros;
            let recent = wall >= recent_from;

            out.ops_total += 1;
            out.bytes_total += bytes;
            if wall > 0 {
                out.oldest_micros = if out.oldest_micros == 0 {
                    wall
                } else {
                    out.oldest_micros.min(wall)
                };
                out.newest_micros = out.newest_micros.max(wall);
            }
            if recent {
                out.ops_last_day += 1;
                out.bytes_last_day += bytes;
            }

            let Some(body) = op.decode_body() else {
                // F0: carried, never classified. It is counted as surviving
                // because this build has no grounds to say otherwise, and
                // over-reporting the wall is the safe direction.
                out.undecodable += 1;
                let row = totals
                    .entry(mesh_proto::UNKNOWN_OP_KIND)
                    .or_insert_with(|| OpKindCount {
                        kind: mesh_proto::UNKNOWN_OP_KIND.to_string(),
                        ..Default::default()
                    });
                row.ops += 1;
                row.bytes += bytes;
                row.surviving += 1;
                out.surviving_total += 1;
                if recent {
                    row.ops_last_day += 1;
                    row.surviving_last_day += 1;
                    out.surviving_last_day += 1;
                }
                continue;
            };

            let kind = body.kind();
            let row = totals.entry(kind).or_insert_with(|| OpKindCount {
                kind: kind.to_string(),
                ..Default::default()
            });
            row.ops += 1;
            row.bytes += bytes;
            if recent {
                row.ops_last_day += 1;
            }

            // Two independent questions, and conflating them was a real
            // bug: *is this the current value for its key* (supersession),
            // and *will its class ever let it go* (time bounds). Both
            // keyed classes have to claim their key even though they never
            // survive — otherwise the walk reaches the previous write of
            // the same entity, finds the key unseen, and counts a
            // superseded op as permanent. `plan` already gets this right;
            // the census has to agree with it or the number it reports is
            // not the number a pass would produce.
            let class = groups
                .class(&body, retention)
                .unwrap_or_else(|| compaction(&body, retention));
            let (key, time_bounded) = match class {
                // Never dead weight, whatever follows it, and it claims no
                // key to supersede anything with.
                Compaction::Grow => {
                    row.surviving += 1;
                    out.surviving_total += 1;
                    if recent {
                        row.surviving_last_day += 1;
                        out.surviving_last_day += 1;
                    }
                    continue;
                }
                Compaction::Lww(k) => (k, false),
                // Both time-bounded classes leave eventually, so neither
                // counts towards what the log costs *for ever* — which is
                // the number this column exists to give. A closed run
                // inside its window is still on disk today and shows up in
                // `ops`; it is not part of the wall.
                Compaction::Decaying { key, .. } | Compaction::Closed { key, .. } => (key, true),
            };
            let survives = seen.insert(key, ()).is_none() && !time_bounded;
            if survives {
                row.surviving += 1;
                out.surviving_total += 1;
                if recent {
                    row.surviving_last_day += 1;
                    out.surviving_last_day += 1;
                }
            }
        }
    }

    out.by_kind = totals.into_values().collect();
    out.by_kind
        .sort_by(|a, b| b.ops.cmp(&a.ops).then_with(|| a.kind.cmp(&b.kind)));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use mesh_proto::Hlc;
    use mesh_proto::{JobId, JobState, JobSummary};

    fn env(seq: u64, wall: u64, body: &OpBody) -> OpEnvelope {
        OpEnvelope {
            author: NodeId([1u8; 32]),
            seq,
            hlc: Hlc {
                wall_micros: wall,
                logical: 0,
            },
            body: postcard::to_stdvec(body).unwrap(),
            signature: vec![0u8; 64],
        }
    }

    /// A policy under which nothing is ever retired.
    ///
    /// Every C0 test below predates M31 and asserts about *supersession*.
    /// Handing them a `forever` window keeps them testing exactly what they
    /// were written to test, and leaves the retirement rule to the M31
    /// tests that follow.
    /// No sink is asking for a hold — which is the default, and what every
    /// test that predates the gate assumes.
    fn no_gate() -> ExportGate {
        ExportGate::default()
    }

    fn keeps_everything() -> RetentionPolicy {
        let mut p = RetentionPolicy::default();
        p.set(
            mesh_proto::RetentionTable::Runs,
            mesh_proto::RetentionWindow::forever(),
        );
        p
    }

    fn job(id: u8, state: JobState) -> OpBody {
        OpBody::JobUpsert {
            summary: JobSummary {
                job_id: JobId([id; 16]),
                cmd_preview: String::new(),
                runtime: "bash".into(),
                submitter: NodeId([1u8; 32]),
                submitter_user: mesh_proto::UserId([1u8; 32]),
                executor: NodeId([1u8; 32]),
                state,
                exit_code: None,
                created_at_micros: 0,
                started_at_micros: None,
                finished_at_micros: None,
                fanout: None,
                updated_hlc: Hlc {
                    wall_micros: 0,
                    logical: 0,
                },
            },
        }
    }

    const DAY_MICROS: u64 = 86_400 * 1_000_000;

    fn window(table: mesh_proto::RetentionTable, days: u32, floor: u32) -> RetentionPolicy {
        let mut p = keeps_everything();
        p.set(
            table,
            mesh_proto::RetentionWindow {
                days,
                floor_days: floor,
            },
        );
        p
    }

    fn fanout(group: FanId) -> [OpBody; 2] {
        let plan = mesh_proto::FanoutPlanRecord {
            group,
            payload: mesh_proto::JobPayload::Command("true".into()),
            env: Vec::new(),
            timeout_secs: None,
            priority: mesh_proto::JobPriority::Normal,
            submitter: NodeId([1u8; 32]),
            submitter_user: mesh_proto::UserId([1u8; 32]),
            auth: mesh_proto::SignedIpc {
                user: mesh_proto::UserId([1u8; 32]),
                at_micros: 0,
                nonce: 0,
                payload: Vec::new(),
                signature: Vec::new(),
            },
            targets: Vec::new(),
            controls: mesh_proto::FanoutControls::default(),
            created_at_micros: 1,
        };
        [
            OpBody::FanoutPlan {
                plan: Box::new(plan),
            },
            OpBody::FanoutClosed {
                group,
                succeeded: 0,
                failed: 0,
                skipped: 0,
            },
        ]
    }

    /// ENG-3: a closed fan-out nobody adopted leaves with its window — plan
    /// and close together — and not before, and not at all unless the
    /// author says it may.
    #[test]
    fn a_retirable_fan_out_leaves_with_its_window() {
        let group = FanId([4; 16]);
        let [p, c] = fanout(group);
        let ops = vec![env(1, 10, &p), env(2, 20, &c)];
        let policy = window(mesh_proto::RetentionTable::Runs, 1, 0);
        let closed_at = 20;
        let groups = RetirableGroups {
            fanouts: [(group, closed_at)].into(),
            gangs: BTreeMap::new(),
        };
        let inside = plan_with(&ops, closed_at + 10, 0, &[], &policy, &no_gate(), &groups);
        assert!(inside.drop.is_empty(), "retired inside its window");
        let after = closed_at + DAY_MICROS + 1;
        let past = plan_with(&ops, after, 0, &[], &policy, &no_gate(), &groups);
        assert_eq!(past.drop, vec![1, 2]);
        assert_eq!(past.retired, 2);
        // Not retirable (adopted, or still open): the plan is kept.
        let kept = plan_with(
            &ops,
            after,
            0,
            &[],
            &policy,
            &no_gate(),
            &RetirableGroups::default(),
        );
        assert!(kept.drop.is_empty());
        // Under a `forever` runs window, nothing retires either.
        let forever = plan_with(
            &ops,
            after,
            0,
            &[],
            &keeps_everything(),
            &no_gate(),
            &groups,
        );
        assert!(forever.drop.is_empty());
    }

    /// ENG-3: the audit window is enforced when it is finite, and only then.
    #[test]
    fn audit_rows_leave_only_under_a_finite_window() {
        let row = OpBody::AuditAppend {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId([1; 16]),
                at_micros: 5,
                user: mesh_proto::UserId([1; 32]),
                username: String::new(),
                action: "job.run".into(),
                target: String::new(),
                node: NodeId([1; 32]),
                outcome: String::new(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        };
        let ops = vec![env(1, 5, &row)];
        let year = 365 * DAY_MICROS;
        let finite = window(mesh_proto::RetentionTable::Audit, 365, 365);
        assert!(
            plan(&ops, year, 0, &[], &finite, &no_gate())
                .drop
                .is_empty()
        );
        assert_eq!(
            plan(&ops, year + 10, 0, &[], &finite, &no_gate()).drop,
            vec![1]
        );
        // The shipped `forever`.
        assert!(
            plan(&ops, 100 * year, 0, &[], &keeps_everything(), &no_gate())
                .drop
                .is_empty()
        );
    }

    #[test]
    fn latest_write_survives_and_earlier_ones_go() {
        let ops = vec![
            env(1, 100, &job(1, JobState::Queued)),
            env(2, 200, &job(1, JobState::Running)),
            env(3, 300, &job(1, JobState::Succeeded)),
        ];
        let p = plan(&ops, 1_000, 0, &[], &keeps_everything(), &no_gate());
        assert_eq!(p.drop, vec![1, 2]);
    }

    #[test]
    fn the_horizon_holds_recent_supersessions() {
        let ops = vec![
            env(1, 100, &job(1, JobState::Queued)),
            env(2, 900, &job(1, JobState::Running)),
            env(3, 950, &job(1, JobState::Succeeded)),
        ];
        // Cutoff 500: seq 1 is old enough, seq 2 is not.
        let p = plan(&ops, 500, 0, &[], &keeps_everything(), &no_gate());
        assert_eq!(p.drop, vec![1]);
        assert_eq!(p.held_by_horizon, 1);
    }

    #[test]
    fn distinct_entities_never_supersede_each_other() {
        let ops = vec![
            env(1, 100, &job(1, JobState::Succeeded)),
            env(2, 200, &job(2, JobState::Succeeded)),
        ];
        assert!(
            plan(&ops, 1_000, 0, &[], &keeps_everything(), &no_gate())
                .drop
                .is_empty()
        );
    }

    #[test]
    fn checkpoints_are_cumulative() {
        let ops = vec![env(9, 100, &job(1, JobState::Succeeded))];
        let p = plan(
            &ops,
            1_000,
            0,
            &[SeqRange::new(1, 4), SeqRange::new(6, 6)],
            &keeps_everything(),
            &no_gate(),
        );
        // **The checkpoint is what has to be cumulative**, and it is built
        // from `ranges()` — which still merges what this pass found with
        // what earlier passes took.
        assert_eq!(p.ranges(), vec![SeqRange::new(1, 4), SeqRange::new(6, 6)]);
        assert_eq!(p.dropped_total(), 5);
        // The split those ranges arrive in (**M-11**): nothing new here,
        // and the prior ranges are carried as ranges rather than expanded
        // into 1, 2, 3, 4, 6 only to be collapsed again.
        assert!(p.drop.is_empty(), "{:?}", p.drop);
        assert_eq!(p.already, vec![SeqRange::new(1, 4), SeqRange::new(6, 6)]);
        // And a seq covered only by the carried ranges is still "covered",
        // which is what stops the pass re-dropping the previous checkpoint.
        assert!(p.covers(3));
        assert!(!p.covers(5));
    }

    #[test]
    fn pruned_set_advances_a_gapless_watermark_across_holes() {
        let set = PrunedSet::from_ranges(&[SeqRange::new(1, 4), SeqRange::new(6, 8)]);
        assert_eq!(set.advance(0), 4);
        assert_eq!(set.advance(5), 8);
        assert_eq!(set.advance(9), 9);
        assert_eq!(set.count(), 7);
    }

    #[test]
    fn adjacent_ranges_coalesce() {
        let mut set = PrunedSet::from_ranges(&[SeqRange::new(1, 4)]);
        assert!(set.insert_all(&[SeqRange::new(5, 9)]));
        assert_eq!(set.ranges(), &[SeqRange::new(1, 9)]);
        assert!(!set.insert_all(&[SeqRange::new(2, 3)]));
    }

    // -- retirement (M31 A1) ----------------------------------------------

    /// A policy with a finite `runs` window.
    fn keeps_runs_for(days: u32) -> RetentionPolicy {
        let mut p = RetentionPolicy::default();
        p.set(
            mesh_proto::RetentionTable::Runs,
            mesh_proto::RetentionWindow {
                days,
                floor_days: 0,
            },
        );
        p
    }

    const DAY: u64 = 86_400 * 1_000_000;

    /// A finished job with a real finish instant.
    fn finished_job(id: u8, at: u64) -> OpBody {
        let mut body = job(id, JobState::Succeeded);
        if let OpBody::JobUpsert { summary } = &mut body {
            summary.finished_at_micros = Some(at);
        }
        body
    }

    fn running_job(id: u8) -> OpBody {
        job(id, JobState::Running)
    }

    /// The core of A1: a closed job leaves the replicated log once the
    /// mesh's window has passed, and one inside it does not.
    #[test]
    fn a_closed_job_past_the_window_is_dropped_and_one_inside_it_is_not() {
        let now = 100 * DAY;
        let old = now - 95 * DAY;
        let recent = now - 5 * DAY;
        let ops = vec![
            env(1, old, &finished_job(1, old)),
            env(2, recent, &finished_job(2, recent)),
        ];

        // Horizon 0 so the *compaction* horizon is not what is being
        // tested: this is purely about the retention window.
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(90), &no_gate());
        assert_eq!(
            p.drop,
            vec![1],
            "the 95-day-old job goes; the 5-day-old stays"
        );
        assert_eq!(p.retired, 1);
        assert_eq!(
            p.decayed, 0,
            "retirement is not decay; the counters are separate"
        );
    }

    /// **A non-terminal job is never dropped whatever its age.** The
    /// `sweep_retention` rule (`jobs.rs`), restated on the replicated side,
    /// and the one that would be a data-loss bug if it were wrong: a job
    /// that has been running for a hundred days is still running.
    #[test]
    fn a_job_that_never_finished_is_never_retired() {
        let now = 1_000 * DAY;
        let ops = vec![env(1, 0, &running_job(1))];
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(1), &no_gate());
        assert!(p.drop.is_empty(), "a running job was dropped: {:?}", p.drop);
        assert_eq!(p.retired, 0);

        // And a terminal summary that somehow carries no finish instant is
        // kept rather than guessed away — the same choice `cleared_by`
        // makes for the same reason.
        let mut body = job(2, JobState::Succeeded);
        if let OpBody::JobUpsert { summary } = &mut body {
            summary.finished_at_micros = None;
        }
        let p = plan(
            &[env(1, 0, &body)],
            now,
            0,
            &[],
            &keeps_runs_for(1),
            &no_gate(),
        );
        assert!(p.drop.is_empty());
    }

    /// `forever` is a thing an operator types, and it means it.
    #[test]
    fn a_forever_window_retires_nothing() {
        let now = 10_000 * DAY;
        let ops = vec![env(1, 0, &finished_job(1, 1))];
        let p = plan(&ops, now, 0, &[], &keeps_everything(), &no_gate());
        assert!(p.drop.is_empty());
        assert_eq!(p.retired, 0);
    }

    /// **The C9 test, and it is the one that matters.** Two nodes with
    /// different node config and the same replicated policy drop exactly
    /// the same ops — because the horizon is a property of the policy and
    /// nothing here can read a config file.
    #[test]
    fn two_nodes_with_the_same_policy_drop_exactly_the_same_ops() {
        let now = 200 * DAY;
        let ops: Vec<OpEnvelope> = (1..=6)
            .map(|i| {
                let at = now - u64::from(i) * 20 * DAY;
                env(u64::from(i), at, &finished_job(i, at))
            })
            .collect();

        let policy = keeps_runs_for(45);
        // The only inputs that differ between two nodes are node config —
        // and the only node-config input `plan` takes is the compaction
        // horizon, which governs *bytes* rather than *what anyone reads*.
        let a = plan(&ops, now, 0, &[], &policy, &no_gate());
        let b = plan(&ops, now, 30 * 24 * 3600, &[], &policy, &no_gate());
        assert_eq!(
            a.drop, b.drop,
            "two differently-configured nodes disagreed about what to drop"
        );
        assert!(
            !a.drop.is_empty(),
            "the fixture must actually retire something"
        );
    }

    /// The audit log is never dropped, at any window, including one that
    /// tries to say zero. `AuditAppend` is `Grow` and A1 adds no knob —
    /// this asserts the property rather than the intention.
    #[test]
    fn the_audit_log_survives_every_retention_window() {
        let now = 10_000 * DAY;
        let ops = vec![env(1, 0, &audit(1)), env(2, 1, &audit(2))];
        for days in [1u32, 90, 0] {
            let p = plan(&ops, now, 0, &[], &keeps_runs_for(days), &no_gate());
            assert!(
                p.drop.is_empty(),
                "an audit entry was dropped at runs={days}: {:?}",
                p.drop
            );
        }
    }

    /// Retirement and supersession compose rather than fighting: the
    /// queued/running ops of one job are dead weight either way, and the
    /// survivor goes too once its window passes.
    #[test]
    fn retirement_and_supersession_compose() {
        let now = 100 * DAY;
        let closed = now - 95 * DAY;
        let ops = vec![
            env(1, closed - 20, &job(1, JobState::Queued)),
            env(2, closed - 10, &job(1, JobState::Running)),
            env(3, closed, &finished_job(1, closed)),
        ];
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(90), &no_gate());
        assert_eq!(
            p.drop,
            vec![1, 2, 3],
            "the whole job leaves, survivor included"
        );
        assert_eq!(
            p.retired, 1,
            "only the survivor was *retired*; the rest were superseded"
        );
    }

    /// **The regression that running it for real found.** Three ops for
    /// one job under a finite window: the survivor is retirable, and the
    /// two before it are superseded. *None* of them is permanent.
    ///
    /// The bug was that a time-bounded class did not claim its compaction
    /// key, so the walk reached the earlier `running` write, found the key
    /// unseen, and counted a superseded op as part of the wall — reporting
    /// one permanent op per job on a mesh where the correct answer is zero.
    /// It is the number the whole of track A is judged on.
    #[test]
    fn a_superseded_write_is_never_counted_as_permanent() {
        let now = 100 * DAY;
        let at = now - DAY;
        let ops = vec![
            env(1, at - 20, &job(1, JobState::Queued)),
            env(2, at - 10, &job(1, JobState::Running)),
            env(3, at, &finished_job(1, at)),
        ];

        let c = census(&ops, now, 0, &keeps_runs_for(90));
        assert_eq!(c.ops_total, 3);
        assert_eq!(
            c.surviving_total, 0,
            "a superseded write was counted towards the wall"
        );

        // And with no window at all, exactly one of the three is permanent
        // — which is C0's whole result, and the number §1.1 models.
        let c = census(&ops, now, 0, &keeps_everything());
        assert_eq!(c.surviving_total, 1);
        assert_eq!(c.by_kind[0].surviving, 1);
    }

    /// The same rule for the decay class, which had the same latent bug: a
    /// usage window republished through an hour is one live counter, not
    /// one per publish.
    #[test]
    fn a_republished_usage_counter_is_one_survivor_not_many() {
        let now = 100 * DAY;
        let window = mesh_proto::usage_window(now);
        let usage = |cpu: u64| OpBody::UsageAdd {
            usage: Box::new(mesh_proto::UsagePartial {
                principal: mesh_proto::UserId([7u8; 32]),
                window,
                cpu_secs: cpu,
                mem_mb_secs: 0,
                gpu_secs: 0,
                wall_secs: cpu,
                jobs: 1,
            }),
        };
        let ops = vec![
            env(1, now - 30, &usage(10)),
            env(2, now - 20, &usage(20)),
            env(3, now - 10, &usage(30)),
        ];
        let c = census(&ops, now, 0, &keeps_everything());
        assert_eq!(c.ops_total, 3);
        // Zero rather than one: a usage window is `Decaying`, so even the
        // live counter leaves eventually and none of it is the wall.
        assert_eq!(c.surviving_total, 0);
    }

    /// A closed job inside the window still counts towards what the log
    /// holds, and still does **not** count towards what it costs for ever —
    /// which is what makes the census move when the policy does.
    #[test]
    fn the_census_stops_counting_a_retirable_job_as_permanent() {
        let now = 100 * DAY;
        let at = now - DAY;
        let ops = vec![env(1, at, &finished_job(1, at))];

        let forever = census(&ops, now, 0, &keeps_everything());
        assert_eq!(forever.surviving_total, 1, "with no window it is permanent");

        let bounded = census(&ops, now, 0, &keeps_runs_for(90));
        assert_eq!(bounded.ops_total, 1, "it is still on disk today");
        assert_eq!(
            bounded.surviving_total, 0,
            "under a window it is not part of the wall"
        );
    }

    // -- the retention gate (M31 B2, §19 Q3) -------------------------------

    fn gate(exported_through: u64, hold_days: u64) -> ExportGate {
        ExportGate {
            exported_through_micros: exported_through,
            hold_micros: hold_days * 86_400 * 1_000_000,
        }
    }

    /// *"Don't delete until it is exported"* — the reasonable half of the
    /// request, and the half that is safe to grant.
    #[test]
    fn a_record_a_sink_has_not_taken_is_held_back() {
        let now = 200 * DAY;
        let closed = now - 95 * DAY;
        let ops = vec![env(1, closed, &finished_job(1, closed))];

        // Sink is behind: the record stays despite being past the window.
        let held = plan(
            &ops,
            now,
            0,
            &[],
            &keeps_runs_for(90),
            &gate(closed - DAY, 30),
        );
        assert!(
            held.drop.is_empty(),
            "a record the sink has not taken was dropped"
        );
        assert_eq!(held.held_for_export, 1);
        assert_eq!(held.retired, 0);
        assert_eq!(held.forced_gaps, 0);

        // Sink has caught up: the ordinary window applies again.
        let freed = plan(&ops, now, 0, &[], &keeps_runs_for(90), &gate(closed, 30));
        assert_eq!(freed.drop, vec![1]);
        assert_eq!(freed.retired, 1);
        assert_eq!(freed.held_for_export, 0);
        assert_eq!(freed.forced_gaps, 0);
    }

    /// **The test that stops C3 eroding through the back door.** Past the
    /// ceiling, deletion resumes whatever the sink has done — and the
    /// permanent gap is counted so it can be said out loud rather than
    /// discovered in a warehouse six months later (§18 decision 03).
    #[test]
    fn past_the_ceiling_deletion_resumes_and_the_gap_is_reported() {
        let closed = 10 * DAY;
        let policy = keeps_runs_for(90);
        let ops = vec![env(1, closed, &finished_job(1, closed))];
        // The sink has never exported anything and never will.
        let stalled = gate(0, 30);

        // Inside window + ceiling: held.
        let now = closed + 100 * DAY;
        let p = plan(&ops, now, 0, &[], &policy, &stalled);
        assert!(p.drop.is_empty());
        assert_eq!(p.held_for_export, 1);

        // Window (90d) + ceiling (30d) = 120 days. One day past that, the
        // mesh's health wins.
        let now = closed + 121 * DAY;
        let p = plan(&ops, now, 0, &[], &policy, &stalled);
        assert_eq!(p.drop, vec![1], "the ceiling did not release the record");
        assert_eq!(p.held_for_export, 0);
        assert_eq!(
            p.forced_gaps, 1,
            "a permanent export gap was created without being counted"
        );
    }

    /// A gate of zero — the default — changes nothing. A hold nobody asked
    /// for that quietly grows the op-log is the opposite of what track A is
    /// for.
    #[test]
    fn no_gate_is_the_default_and_holds_nothing() {
        let now = 200 * DAY;
        let closed = now - 95 * DAY;
        let ops = vec![env(1, closed, &finished_job(1, closed))];
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(90), &no_gate());
        assert_eq!(p.drop, vec![1]);
        assert_eq!(p.held_for_export, 0);
        assert_eq!(p.forced_gaps, 0);
    }

    /// The gate never holds what the window would have kept anyway, and it
    /// never resurrects what supersession already condemned: the
    /// queued/running writes of a gated job are still dead weight.
    #[test]
    fn the_gate_holds_survivors_not_superseded_writes() {
        let now = 200 * DAY;
        let closed = now - 95 * DAY;
        let ops = vec![
            env(1, closed - 20, &job(1, JobState::Queued)),
            env(2, closed - 10, &job(1, JobState::Running)),
            env(3, closed, &finished_job(1, closed)),
        ];
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(90), &gate(0, 30));
        assert_eq!(
            p.drop,
            vec![1, 2],
            "the gate should hold only the surviving record"
        );
        assert_eq!(p.held_for_export, 1);
    }

    /// A gate cannot save the audit log from being `Grow`, and cannot
    /// condemn it either — it applies to `Closed` and to nothing else.
    #[test]
    fn the_gate_touches_only_closed_records() {
        let now = 10_000 * DAY;
        let ops = vec![env(1, 0, &audit(1)), env(2, 1, &audit(2))];
        let p = plan(&ops, now, 0, &[], &keeps_runs_for(1), &gate(0, 30));
        assert!(p.drop.is_empty());
        assert_eq!(p.held_for_export, 0, "an audit entry was counted as gated");
    }

    // -- the census (M31 P0) ---------------------------------------------

    fn audit(id: u8) -> OpBody {
        OpBody::AuditAppend {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId([id; 16]),
                at_micros: 0,
                user: mesh_proto::UserId([0u8; 32]),
                username: String::new(),
                action: "secret.resolve".into(),
                target: String::new(),
                node: NodeId([0u8; 32]),
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        }
    }

    /// A log of known composition, counted. Three ops for one job of which
    /// one survives, plus two audit entries that never compact at all —
    /// which is exactly §1.1's model, at a scale that fits in a test.
    #[test]
    fn the_census_separates_what_survives_from_what_the_log_holds() {
        let now = 10_000_000;
        let ops = vec![
            env(1, now - 100, &job(1, JobState::Queued)),
            env(2, now - 90, &job(1, JobState::Running)),
            env(3, now - 80, &job(1, JobState::Succeeded)),
            env(4, now - 70, &audit(1)),
            env(5, now - 60, &audit(2)),
        ];
        let c = census(&ops, now, 4096, &keeps_everything());

        assert_eq!(c.ops_total, 5);
        // One surviving `JobUpsert`, plus both audit entries: the audit log
        // is `Grow` and nothing will ever collapse it.
        assert_eq!(c.surviving_total, 3);
        assert_eq!(c.disk_bytes, 4096);
        assert!(c.bytes_total > 0);

        let by = |kind: &str| {
            c.by_kind
                .iter()
                .find(|k| k.kind == kind)
                .unwrap_or_else(|| panic!("no {kind} row"))
        };
        assert_eq!(by("JobUpsert").ops, 3);
        assert_eq!(by("JobUpsert").surviving, 1);
        assert_eq!(by("AuditAppend").ops, 2);
        assert_eq!(by("AuditAppend").surviving, 2);

        // Descending by op count, so `top_kinds` is the answer to "what is
        // filling my log" without the caller sorting anything.
        assert_eq!(c.top_kinds(1)[0].kind, "JobUpsert");
    }

    /// Supersession is per author. Two nodes each finishing their own job
    /// supersede nothing of each other's, and a census that resolved keys
    /// globally would under-report the wall by the size of the fleet.
    #[test]
    fn the_census_resolves_supersession_per_author_not_globally() {
        let now = 10_000_000;
        let mut ops = vec![
            env(1, now - 100, &job(1, JobState::Queued)),
            env(2, now - 90, &job(1, JobState::Succeeded)),
        ];
        for op in &mut ops[..] {
            op.author = NodeId([1u8; 32]);
        }
        let mut theirs = vec![
            env(1, now - 100, &job(1, JobState::Queued)),
            env(2, now - 90, &job(1, JobState::Succeeded)),
        ];
        for op in &mut theirs[..] {
            op.author = NodeId([2u8; 32]);
        }
        ops.extend(theirs);

        let c = census(&ops, now, 0, &keeps_everything());
        assert_eq!(c.ops_total, 4);
        assert_eq!(
            c.surviving_total, 2,
            "one survivor per author, not one total"
        );
    }

    /// The growth rate is measured over the last day, and the settling
    /// projection is derived from the *surviving* share of it — because the
    /// total is what the log costs this week and the survivors are what it
    /// costs for ever.
    #[test]
    fn growth_is_measured_over_the_last_day_and_projects_from_survivors() {
        let day = CENSUS_WINDOW_MICROS;
        let now = 10 * day;
        let ops = vec![
            // Last year: superseded, and outside the window either way.
            env(1, now - 5 * day, &job(1, JobState::Queued)),
            env(2, now - 5 * day, &job(1, JobState::Succeeded)),
            // Today: one job, three ops, one survivor.
            env(3, now - 100, &job(2, JobState::Queued)),
            env(4, now - 90, &job(2, JobState::Running)),
            env(5, now - 80, &job(2, JobState::Succeeded)),
        ];
        let c = census(&ops, now, 0, &keeps_everything());

        assert_eq!(c.ops_last_day, 3);
        assert_eq!(c.surviving_last_day, 1);
        assert_eq!(c.ops_total, 5);
        assert_eq!(c.surviving_total, 2);

        // One survivor a day at the mean envelope size.
        let mean = c.bytes_last_day / 3;
        assert_eq!(c.surviving_bytes_per_day(), mean);
        assert_eq!(c.settles_at_bytes(90), Some(mean * 90));
        // `0 = forever` does not settle, and says so rather than
        // reporting zero.
        assert_eq!(c.settles_at_bytes(0), None);
    }

    /// An empty log is not a division by zero.
    #[test]
    fn an_empty_log_censuses_to_zero_without_panicking() {
        let c = census(&[], 1_000_000, 0, &keeps_everything());
        assert_eq!(c.ops_total, 0);
        assert_eq!(c.surviving_bytes_per_day(), 0);
        assert!(c.by_kind.is_empty());
        assert!(c.top_kinds(3).is_empty());
    }

    /// An op from a newer protocol is counted, not omitted — otherwise the
    /// census stops matching the disk on exactly the mesh that is mid
    /// upgrade (F0).
    #[test]
    fn an_undecodable_op_is_counted_rather_than_skipped() {
        let mut op = env(1, 100, &job(1, JobState::Succeeded));
        // A variant index far past anything this build has.
        op.body = vec![0xfe, 0x7f, 0x00, 0x00];
        let c = census(&[op], 1_000, 0, &keeps_everything());
        assert_eq!(c.ops_total, 1);
        assert_eq!(c.undecodable, 1);
        assert_eq!(
            c.surviving_total, 1,
            "assume it counts, never assume it does not"
        );
        assert_eq!(c.by_kind[0].kind, mesh_proto::UNKNOWN_OP_KIND);
    }

    /// ENG-18. Two rewrap forms, two classes, and getting the older one
    /// wrong loses the ability to read a secret rather than a byte of log.
    ///
    /// A secret's wraps are the only way a node can open it. The current
    /// form restates everything its author provides, so a later one really
    /// does supersede an earlier one. The pre-SEC-17 form carried only the
    /// recipients that were missing at the time, and the apply path adds
    /// those to what it already has — so collapsing them by the same key
    /// keeps the last op's recipients and silently discards the rest. The
    /// author would not notice: its live state was built before the drop.
    /// A restart, a joiner or a restore replays what is left.
    #[test]
    fn only_a_rewrap_that_restates_everything_may_supersede_an_earlier_one() {
        let author = NodeId([7u8; 32]);
        let recipient = NodeId([8u8; 32]);
        let wrap = |node: NodeId| mesh_proto::SecretWrap {
            node_id: node,
            eph_pub: [1u8; 32],
            nonce: [2u8; 24],
            wrapped_key: vec![3u8; 48],
        };
        let rewrap = |wraps: Vec<mesh_proto::SecretWrap>| OpBody::SecretRewrap {
            name: "db-password".into(),
            version: 3,
            wraps,
        };

        // Current form: carries the author's statement marker.
        let total = rewrap(vec![
            wrap(recipient),
            crate::secrets::statement_marker(author, 0),
        ]);
        assert_eq!(
            compaction(&total, &keeps_everything()),
            Compaction::Lww(CompactKey("secret-rewrap", "db-password@3".into())),
        );

        // Older form: no marker, additive, and therefore permanent.
        assert_eq!(
            compaction(&rewrap(vec![wrap(recipient)]), &keeps_everything()),
            Compaction::Grow,
            "an additive rewrap that is superseded is a recipient that can no \
             longer open the secret"
        );
    }

    /// ENG-19 — the `facts` window was the one retention knob with no
    /// enforcement anywhere. LWW bounds a subject that is *still* being
    /// collected and does nothing for one that is not: a host taken out of
    /// the inventory, a collector an admin removed, a machine that left.
    /// Each left its last document in the log for the life of the mesh.
    #[test]
    fn a_fact_nobody_republishes_leaves_with_the_facts_window() {
        let doc = |collected_at_micros: u64| OpBody::FactsPublish {
            subject: mesh_proto::FactSubject::Node(NodeId([4u8; 32])),
            collector: "os".into(),
            doc: b"{}".to_vec(),
            digest: [0u8; 32],
            collected_at_micros,
        };
        let day = 86_400u64 * 1_000_000;

        // Under `forever` it is plain LWW: an operator typed that.
        let mut forever = RetentionPolicy::default();
        forever.set(
            mesh_proto::RetentionTable::Facts,
            mesh_proto::RetentionWindow::forever(),
        );
        assert!(matches!(
            compaction(&doc(100 * day), &forever),
            Compaction::Lww(_)
        ));

        // With a window it decays from the instant the collector observed
        // it — so a document that is still being republished is never at
        // risk, and one that is not eventually goes.
        let mut thirty = RetentionPolicy::default();
        thirty.set(
            mesh_proto::RetentionTable::Facts,
            mesh_proto::RetentionWindow {
                days: 30,
                floor_days: 0,
            },
        );
        let Compaction::Decaying {
            key,
            expires_micros,
        } = compaction(&doc(100 * day), &thirty)
        else {
            panic!("a facts window must decay its documents");
        };
        assert_eq!(key.0, "facts");
        assert_eq!(
            expires_micros,
            130 * day,
            "a document expires a window after it was collected"
        );
    }

    #[test]
    fn the_audit_log_never_compacts() {
        // The single most important line in this file.
        let body = OpBody::AuditAppend {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId([0u8; 16]),
                at_micros: 0,
                user: mesh_proto::UserId([0u8; 32]),
                username: String::new(),
                action: "job.run".into(),
                target: String::new(),
                node: NodeId([0u8; 32]),
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        };
        assert_eq!(compaction(&body, &keeps_everything()), Compaction::Grow);
    }
}
