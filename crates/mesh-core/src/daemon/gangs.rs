//! Gang scheduling: all-or-nothing reservations across several machines.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2). The file was 31,056
//! lines and `impl Node` was 27,994 of them; this section was already
//! marked off by a banner comment, which is the cheapest kind of seam to
//! cut along — somebody had already decided it was one topic.

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    // -----------------------------------------------------------------
    // C4 gangs (C0-C9 §8)
    // -----------------------------------------------------------------
    /// The client-facing gang driver: plan, replicate, negotiate, run.
    ///
    /// Structurally the fan-out driver with one phase in front of it — and
    /// that phase is the milestone. Everything after the commit is the
    /// fan-out's multiplexed streaming, unchanged, because once every
    /// member has started the two are the same thing.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_gang(
        &self,
        user: UserId,
        auth: SignedIpc,
        payload: JobPayload,
        env: Vec<(String, String)>,
        timeout_secs: Option<u64>,
        priority: JobPriority,
        size: u32,
        ranked: Vec<MemberInfo>,
        want: outputs::Wanted,
        expect: Option<crate::results::Expectation>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let me = self.node_id();
        let reqs = mesh_proto::Requirements::from_env(&env);
        let placement = env
            .iter()
            .find(|(k, _)| k == mesh_proto::REQ_GANG_PLACEMENT)
            .and_then(|(_, v)| mesh_proto::GangPlacement::parse(v))
            .unwrap_or_default();
        let restart = env
            .iter()
            .any(|(k, v)| k == mesh_proto::REQ_GANG_RESTART && matches!(v.as_str(), "1" | "true"));

        // The 1.12 safety gate (see `CLUSTER_MINOR`). A node that cannot
        // hold a lease must never be offered one: believing a reservation
        // exists when nothing holds it is how a gang starts N-1 of N, which
        // is the exact failure all-or-nothing admission prevents. So the
        // candidate set is narrowed *before* the plan is built, and a mesh
        // that is too old is told so by version rather than handed a
        // partial gang.
        let old: Vec<String> = ranked
            .iter()
            .filter(|m| {
                m.node_id != me
                    && self
                        .inner
                        .transport
                        .peer_minor(&m.node_id)
                        .is_some_and(|minor| minor < mesh_proto::CLUSTER_MINOR)
            })
            .map(|m| m.name.clone())
            .collect();
        let ranked: Vec<MemberInfo> = ranked
            .into_iter()
            .filter(|m| !old.contains(&m.name))
            .collect();

        let assigned = Self::assign_gang(&ranked, size, &reqs, placement);
        if (assigned.len() as u32) < size {
            let mut message = format!(
                "a gang of {size} needs {size} simultaneous {} — this mesh offers {}",
                if reqs.cpus > 0 {
                    format!("slots of {} core(s)", reqs.cpus)
                } else {
                    "slots".to_string()
                },
                assigned.len()
            );
            if !old.is_empty() {
                message.push_str(&format!(
                    "; {} node(s) predate protocol 1.{} and cannot hold a \
                     reservation ({})",
                    old.len(),
                    mesh_proto::CLUSTER_MINOR,
                    old.join(", ")
                ));
            }
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        }

        let gang = mesh_proto::GangId::generate();
        let plan = mesh_proto::GangPlanRecord {
            gang,
            ticket: mesh_proto::GangTicket::new(priority, self.inner.engine.clock().now(), gang),
            payload,
            env,
            timeout_secs,
            priority,
            submitter: me,
            submitter_user: user,
            auth: auth.clone(),
            members: assigned
                .iter()
                .map(|m| mesh_proto::GangMember {
                    job_id: mesh_proto::JobId::generate(),
                    node_id: m.node_id,
                    node_name: m.name.clone(),
                    addr: Self::gang_addr(m),
                })
                .collect(),
            placement,
            restart,
            attempt: 0,
            created_at_micros: mesh_proto::now_micros(),
        };

        // Replicate before the first offer. From this moment a successor
        // can finish the gang if we die, and — because no lease is
        // replicated — the only question it has to answer is whether this
        // gang ever committed, which the members' job summaries answer.
        if let Err(e) = self.inner.engine.author_op(OpBody::GangPlan {
            plan: Box::new(plan.clone()),
        }) {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("cannot record gang plan: {e}"),
                })
                .await;
            return;
        }

        audit::record(
            &self.inner.engine,
            user,
            "job.run",
            &gang.to_display_string(),
            "ok",
            &format!(
                "gang {} × {} ({})",
                size,
                Self::gang_spec(&plan, 0).preview(),
                placement.as_str()
            ),
            &auth.signature,
        );

        self.drive_gang(plan, Some((out, want, expect))).await;
    }

    /// Negotiate and run a gang, with or without a client on the far end.
    ///
    /// One function for both because a gang adopted after its origin died
    /// has to do *exactly* what the origin was doing, and the surest way to
    /// guarantee that is for it to be the same code. The client half is an
    /// `Option`: everything it does is emit frames.
    #[tracing::instrument(
        level = "info",
        name = "gang",
        skip_all,
        fields(gang = %plan.gang.short())
    )]
    pub(super) async fn drive_gang(
        &self,
        plan: mesh_proto::GangPlanRecord,
        client: Option<(
            mpsc::Sender<IpcResponse>,
            outputs::Wanted,
            Option<crate::results::Expectation>,
        )>,
    ) {
        let (out, want, expect) = match client {
            Some((out, want, expect)) => (Some(out), want, expect),
            None => (None, outputs::Wanted::default(), None),
        };
        // **One claim, held across the whole restart chain.** The marker is
        // what stops the adoption scan spawning a second driver for a gang
        // this node is already driving, so releasing it between attempts —
        // even for the instant it takes to author the next plan — would
        // open exactly the window it exists to close.
        //
        // `claimed` is the *current* attempt's id: a restart is a new gang,
        // so the claim moves with it, atomically, under one lock.
        let mut plan = plan;
        let mut claimed = plan.gang;
        if !self.claim_gang(claimed) {
            return; // already being driven here
        }
        // One run, however many attempts `--gang-restart` takes (ENG-21).
        // Counting per attempt would report a gang that lost a machine
        // twice as three gangs, and the number an operator wants is how
        // many gangs they asked for.
        self.inner.groups.started(crate::observe::GroupKind::Gang);
        let began = Instant::now();
        loop {
            let attempt = self
                .drive_gang_attempt(&plan, out.clone(), &want, expect.as_ref())
                .await;
            let outcome = attempt.outcome;
            // `--gang-restart` (§8.2). A restart is a **new gang** rather
            // than a re-dispatch of this one: the members that did not die
            // have journaled their job ids, and an executor treats a
            // journaled id as accepted — re-sending them would ack
            // instantly and run nothing.
            //
            // Only a member *death* restarts. A gang that never got its
            // reservations did not fail, it did not start, and re-running
            // the negotiation is what the offer rounds already are.
            if outcome != mesh_proto::GangOutcome::MemberDied
                || !plan.restart
                || plan.attempt + 1 >= mesh_proto::MAX_GANG_ATTEMPTS
            {
                // The terminal frame is emitted here rather than inside the
                // attempt, because an attempt that is about to be retried
                // is not the end of anything and telling the client it was
                // would be a lie the client acts on.
                if let Some(out) = &out {
                    if plan.restart && outcome == mesh_proto::GangOutcome::MemberDied {
                        let _ = out
                            .send(IpcResponse::Notice {
                                message: format!(
                                    "gang restarted {} time(s) and kept losing a member; \
                                     giving up",
                                    plan.attempt
                                ),
                            })
                            .await;
                    }
                    match attempt.detail {
                        Some(message) => {
                            let _ = out.send(IpcResponse::Error { message }).await;
                        }
                        None => {
                            let _ = out
                                .send(IpcResponse::GangDone {
                                    gang: plan.gang,
                                    outcome,
                                    succeeded: attempt.succeeded,
                                    failed: attempt.failed,
                                })
                                .await;
                        }
                    }
                }
                self.inner.groups.finished(
                    crate::observe::GroupKind::Gang,
                    match outcome {
                        mesh_proto::GangOutcome::Succeeded => {
                            crate::observe::GroupOutcome::Completed
                        }
                        mesh_proto::GangOutcome::Failed | mesh_proto::GangOutcome::MemberDied => {
                            crate::observe::GroupOutcome::Failed
                        }
                        // It never ran, which is the guarantee the
                        // milestone is for: a gang that could not seat
                        // every member stopped on purpose and is not a
                        // failure of the work.
                        mesh_proto::GangOutcome::NeverStarted => {
                            crate::observe::GroupOutcome::Halted
                        }
                    },
                    began.elapsed(),
                    u64::from(attempt.succeeded),
                    u64::from(attempt.failed),
                    // Members that never became jobs: the whole gang, when
                    // it never started.
                    u64::try_from(plan.members.len())
                        .unwrap_or(u64::MAX)
                        .saturating_sub(u64::from(attempt.succeeded))
                        .saturating_sub(u64::from(attempt.failed)),
                );
                self.release_gang(claimed);
                return;
            }
            let next = mesh_proto::GangPlanRecord {
                gang: mesh_proto::GangId::generate(),
                attempt: plan.attempt + 1,
                members: plan
                    .members
                    .iter()
                    .map(|m| mesh_proto::GangMember {
                        job_id: mesh_proto::JobId::generate(),
                        ..m.clone()
                    })
                    .collect(),
                created_at_micros: mesh_proto::now_micros(),
                ..plan.clone()
            };
            // Seniority is inherited from the *original* submission, not
            // stamped fresh: a gang that lost a machine and had to start
            // over should not also lose its place to everything submitted
            // while it was running.
            let next = mesh_proto::GangPlanRecord {
                ticket: mesh_proto::GangTicket::new(plan.priority, plan.ticket.submit, next.gang),
                ..next
            };
            if let Err(e) = self.inner.engine.author_op(OpBody::GangPlan {
                plan: Box::new(next.clone()),
            }) {
                warn!(gang = %next.gang.short(), "cannot record gang restart: {e}");
                self.release_gang(claimed);
                return;
            }
            self.move_claim(claimed, next.gang);
            claimed = next.gang;
            if let Some(out) = &out {
                let _ = out
                    .send(IpcResponse::Notice {
                        message: format!(
                            "a member died; restarting the gang as {} (attempt {})",
                            next.gang.short(),
                            next.attempt + 1
                        ),
                    })
                    .await;
            }
            plan = next;
        }
    }

    /// Take the marker that says this node is driving `gang`, or report
    /// that somebody here already is.
    ///
    /// `HashSet::insert`'s return value under one lock, which is the whole
    /// of the mutual exclusion: the adoption scan checks the same set, so a
    /// claim taken here is a driver the scan will not duplicate.
    pub(super) fn claim_gang(&self, gang: mesh_proto::GangId) -> bool {
        self.inner.active_gangs.lock().insert(gang)
    }

    pub(super) fn release_gang(&self, gang: mesh_proto::GangId) {
        self.inner.active_gangs.lock().remove(&gang);
    }

    /// Hand the claim from a gang to its restart, **under one lock**, so
    /// there is no instant in which neither is claimed and the adoption
    /// scan could start a second driver.
    pub(super) fn move_claim(&self, from: mesh_proto::GangId, to: mesh_proto::GangId) {
        let mut held = self.inner.active_gangs.lock();
        held.remove(&from);
        held.insert(to);
    }

    /// One attempt at a gang: negotiate, commit, run, report.
    pub(super) async fn drive_gang_attempt(
        &self,
        plan: &mesh_proto::GangPlanRecord,
        out: Option<mpsc::Sender<IpcResponse>>,
        want: &outputs::Wanted,
        expect: Option<&crate::results::Expectation>,
    ) -> GangAttempt {
        let gang = plan.gang;
        let say = |frame: IpcResponse| {
            let out = out.clone();
            async move {
                if let Some(out) = out {
                    let _ = out.send(frame).await;
                }
            }
        };

        // A gang this driver has adopted may still have reservations out
        // from the dead origin's last round. Releasing blindly is safe
        // (release is idempotent per gang) and it is the difference between
        // one wasted round and one wasted lease lifetime.
        if plan.submitter != self.node_id() {
            self.gang_release_all(plan).await;
        }

        let mut rounds = 0u32;
        let mut committed = false;
        let mut last_reason = String::new();
        while rounds < mesh_proto::MAX_OFFER_ROUNDS {
            if *self.inner.shutdown_tx.borrow() {
                self.gang_release_all(plan).await;
                // A successor (or we, after restart) re-drives. Nothing is
                // closed here, deliberately: the gang is not over, this
                // node is.
                return GangAttempt::stopped(mesh_proto::GangOutcome::NeverStarted, None);
            }
            rounds += 1;
            match self.gang_round(plan).await {
                GangRoundOutcome::Committed => {
                    committed = true;
                    break;
                }
                GangRoundOutcome::Refused {
                    held,
                    behind,
                    reason,
                } => {
                    last_reason = reason;
                    say(IpcResponse::GangWaiting {
                        gang,
                        held,
                        size: plan.size(),
                        round: rounds,
                        behind,
                    })
                    .await;
                    // Backoff exists to stop hammering, not to break ties —
                    // seniority does that, which is why this is a short
                    // fixed wait rather than an exponential one. A junior
                    // gang that backed off exponentially behind a senior
                    // one would take minutes to notice it had been let
                    // through.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            }
        }

        if !committed {
            warn!(gang = %gang.short(), rounds, "gang never started: {last_reason}");
            self.close_gang(gang, mesh_proto::GangOutcome::NeverStarted, rounds);
            return GangAttempt::stopped(
                mesh_proto::GangOutcome::NeverStarted,
                Some(format!(
                    "gang {} never started after {rounds} offer round(s): {last_reason}",
                    gang.short()
                )),
            );
        }

        info!(gang = %gang.short(), size = plan.size(), rounds, "gang committed");
        let dispatch = self.gang_dispatch(plan).await;
        let failed_dispatch: Vec<String> = dispatch
            .iter()
            .enumerate()
            .filter_map(|(rank, r)| {
                r.as_ref()
                    .err()
                    .map(|e| format!("{}: {e}", plan.members[rank].node_name))
            })
            .collect();

        say(IpcResponse::GangStarted {
            gang,
            members: plan
                .members
                .iter()
                .map(|m| FanTarget {
                    job_id: m.job_id,
                    node_id: m.node_id,
                    node_name: m.node_name.clone(),
                })
                .collect(),
            rounds,
        })
        .await;

        if !failed_dispatch.is_empty() {
            // The reservations were all held and a member still failed to
            // start — a bundle that would not fetch, a node that died in
            // the last millisecond. There is no half-gang, so the rest come
            // down with it.
            warn!(gang = %gang.short(), "gang dispatch failed: {}", failed_dispatch.join("; "));
            self.cancel_gang(plan).await;
            self.close_gang(gang, mesh_proto::GangOutcome::MemberDied, rounds);
            return GangAttempt::stopped(
                mesh_proto::GangOutcome::MemberDied,
                Some(format!(
                    "gang {} could not start every member ({}); the rest were cancelled",
                    gang.short(),
                    failed_dispatch.join("; ")
                )),
            );
        }

        let attempt = self.watch_gang(plan, out.clone(), want, expect).await;
        self.close_gang(gang, attempt.outcome, rounds);
        attempt
    }

    /// Stream every member's logs, and enforce the member-death rule.
    ///
    /// **A gang with a dead member is a gang with a hung barrier**, so the
    /// default is to tear the whole thing down rather than leave the
    /// survivors blocked on a rendezvous that will never complete. Partial
    /// survival is not offered because nothing in the workload class
    /// tolerates it.
    pub(super) async fn watch_gang(
        &self,
        plan: &mesh_proto::GangPlanRecord,
        out: Option<mpsc::Sender<IpcResponse>>,
        want: &outputs::Wanted,
        expect: Option<&crate::results::Expectation>,
    ) -> GangAttempt {
        let (mtx, mut mrx) = mpsc::channel::<IpcResponse>(256);
        for rank in 0..plan.members.len() {
            self.spawn_gang_member_stream(plan, rank, want, mtx.clone());
        }
        drop(mtx);

        let mut succeeded = 0u32;
        let mut failed = 0u32;
        let mut torn_down = false;
        let mut rows: Vec<mesh_proto::ResultRow> = Vec::new();
        let mut client_gone = out.is_none();
        while let Some(frame) = mrx.recv().await {
            if let IpcResponse::FanJobDone { state, .. } = &frame {
                if *state == mesh_proto::JobState::Succeeded {
                    succeeded += 1;
                } else {
                    failed += 1;
                    // The first death ends the gang. `--gang-restart` is
                    // handled a level up, by the adoption scan re-driving
                    // the plan; here the job is to stop the survivors
                    // burning GPUs on a barrier nobody will reach.
                    if !torn_down && (succeeded + failed) < plan.size() {
                        torn_down = true;
                        info!(gang = %plan.gang.short(),
                            "a member died; tearing the gang down");
                        self.cancel_gang(plan).await;
                    }
                }
            }
            let frame = match frame {
                IpcResponse::Result(mut row) => {
                    if let Some(expect) = expect {
                        expect.judge(&mut row);
                    }
                    rows.push((*row).clone());
                    IpcResponse::Result(row)
                }
                other => other,
            };
            if !client_gone
                && let Some(out) = &out
                && out.send(frame).await.is_err()
            {
                client_gone = true;
            }
        }

        let outcome = if torn_down {
            mesh_proto::GangOutcome::MemberDied
        } else if failed == 0 {
            mesh_proto::GangOutcome::Succeeded
        } else {
            mesh_proto::GangOutcome::Failed
        };

        if !want.is_empty() {
            let set = mesh_proto::ResultSet {
                group: mesh_proto::FanId(plan.gang.0),
                owner: self.node_id(),
                owner_name: self.member_name(&self.node_id()),
                submitter_user: plan.submitter_user,
                preview: Self::gang_spec(plan, 0).preview(),
                created_at_micros: plan.created_at_micros,
                missing: 0,
                rows: rows.clone(),
            };
            self.record_results(&set);
        }
        if let (Some(out), Some(expect)) = (&out, expect)
            && !client_gone
        {
            let bad: Vec<String> = rows
                .iter()
                .filter(|r| r.expect_passed == Some(false))
                .map(|r| format!("{}: {}", r.node_name, r.expect_detail))
                .collect();
            let passed = u32::try_from(rows.len() - bad.len()).unwrap_or(u32::MAX);
            let _ = out
                .send(IpcResponse::Expected {
                    expression: expect.source.clone(),
                    passed,
                    failed: bad,
                })
                .await;
        }
        GangAttempt {
            outcome,
            succeeded,
            failed,
            detail: None,
        }
    }

    /// Pump one member's log stream into the shared channel, attributed by
    /// rank. The fan-out's per-target streamer with the index fixed to the
    /// member's rank — the framing is shared deliberately, so there is one
    /// multiplexed renderer to keep correct rather than two.
    pub(super) fn spawn_gang_member_stream(
        &self,
        plan: &mesh_proto::GangPlanRecord,
        rank: usize,
        want: &outputs::Wanted,
        mtx: mpsc::Sender<IpcResponse>,
    ) {
        let member = plan.members[rank].clone();
        let index = u32::try_from(rank).unwrap_or(u32::MAX);
        let mut capture = crate::results::RowCapture::new(want);
        let this = self.clone();
        let handle = tokio::spawn(async move {
            let job_id = member.job_id;
            let (ttx, mut trx) = mpsc::channel::<IpcResponse>(64);
            let streamer = this.clone();
            let target = member.node_id;
            tokio::spawn(async move {
                streamer.stream_job_logs(job_id, target, true, ttx).await;
            });
            let finish = |cap: crate::results::RowCapture,
                          state: mesh_proto::JobState,
                          exit_code: Option<i32>,
                          this: &Node| {
                cap.finish(
                    job_id,
                    member.node_id,
                    member.node_name.clone(),
                    state,
                    exit_code,
                    Some(mesh_proto::now_micros()),
                    |key, value| this.spill_output(job_id, key, value),
                )
            };
            while let Some(frame) = trx.recv().await {
                match frame {
                    IpcResponse::LogChunk { stream, data } => {
                        if let Some(cap) = capture.as_mut() {
                            cap.chunk(stream, &data);
                        }
                        if mtx
                            .send(IpcResponse::FanChunk {
                                index,
                                stream,
                                data,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    IpcResponse::JobDone { state, exit_code } => {
                        if let Some(cap) = capture.take() {
                            let row = finish(cap, state, exit_code, &this);
                            let _ = mtx.send(IpcResponse::Result(Box::new(row))).await;
                        }
                        let _ = mtx
                            .send(IpcResponse::FanJobDone {
                                index,
                                state,
                                exit_code,
                            })
                            .await;
                        return;
                    }
                    IpcResponse::Error { message } => {
                        let detail = format!("meshware: {message}\n");
                        if let Some(mut cap) = capture.take() {
                            cap.chunk(2, detail.as_bytes());
                            let row = finish(cap, mesh_proto::JobState::Failed, None, &this);
                            let _ = mtx.send(IpcResponse::Result(Box::new(row))).await;
                        }
                        let _ = mtx
                            .send(IpcResponse::FanChunk {
                                index,
                                stream: 2,
                                data: detail.into_bytes(),
                            })
                            .await;
                        let _ = mtx
                            .send(IpcResponse::FanJobDone {
                                index,
                                state: mesh_proto::JobState::Failed,
                                exit_code: None,
                            })
                            .await;
                        return;
                    }
                    _ => {}
                }
            }
            // The stream ended without a terminal frame: the executor died
            // mid-job. For a gang that is a dead member, and a dead member
            // is the end of the gang.
            if let Some(cap) = capture.take() {
                let row = finish(cap, mesh_proto::JobState::Failed, None, &this);
                let _ = mtx.send(IpcResponse::Result(Box::new(row))).await;
            }
            let _ = mtx
                .send(IpcResponse::FanJobDone {
                    index,
                    state: mesh_proto::JobState::Failed,
                    exit_code: None,
                })
                .await;
        });
        self.track(handle);
    }

    /// Cancel every member of a gang, wherever it is running.
    pub(super) async fn cancel_gang(&self, plan: &mesh_proto::GangPlanRecord) {
        for member in &plan.members {
            if member.node_id == self.node_id() {
                let _ = self.inner.jobs.cancel(&member.job_id);
                continue;
            }
            if let Ok(conn) = self.remote_conn(member.node_id).await {
                let _ = self
                    .inner
                    .transport
                    .request_one(
                        &conn,
                        &Request::CancelJob {
                            job_id: member.job_id,
                        },
                    )
                    .await;
            }
        }
        self.gang_release_all(plan).await;
    }

    pub(super) fn close_gang(
        &self,
        gang: mesh_proto::GangId,
        outcome: mesh_proto::GangOutcome,
        rounds: u32,
    ) {
        if let Err(e) = self.inner.engine.author_op(OpBody::GangClosed {
            gang,
            outcome,
            rounds,
        }) {
            warn!(gang = %gang.short(), "cannot close gang: {e}");
        }
    }

    /// Adopt gangs whose owner is gone (§8.2 P5).
    ///
    /// The fan-out scan's twin, with one extra decision that is the whole
    /// point of the milestone's failure model: a gang that **never
    /// committed** is re-driven from scratch, because its leases have
    /// already expired on their own and nothing needs cleaning up. A gang
    /// that *had* committed has running jobs, and re-driving it would
    /// double-run them — so the successor watches instead, and applies the
    /// member-death rule the dead origin can no longer apply.
    pub(super) fn scan_adoptable_gangs(&self) {
        let me = self.node_id();
        let since = self.drivable_since();
        let open: Vec<(mesh_proto::GangPlanRecord, NodeId, bool)> = self.inner.engine.read(|s| {
            s.gangs
                .values()
                .filter(|e| e.closed.is_none() && e.plan.created_at_micros >= since)
                .map(|e| (e.plan.clone(), e.owner(), e.committed(&s.jobs)))
                .collect()
        });
        for (plan, owner, committed) in open {
            let gang = plan.gang;
            if self.inner.active_gangs.lock().contains(&gang) {
                continue; // already driving it (as origin or adopter)
            }
            if owner == me {
                info!(gang = %gang.short(), "resuming own gang after restart");
                self.clone().spawn_gang_drive(plan, committed);
                continue;
            }
            let owner_gone = self.inner.engine.read(|s| !s.is_active_member(&owner))
                || matches!(self.inner.gossip.liveness_of(&owner), Liveness::Dead);
            if !owner_gone {
                continue;
            }
            let live: Vec<NodeId> = self.inner.engine.read(|s| {
                s.members
                    .values()
                    .filter(|m| s.is_active_member(&m.info.node_id))
                    .map(|m| m.info.node_id)
                    .filter(|id| {
                        *id != owner && matches!(self.inner.gossip.liveness_of(id), Liveness::Alive)
                    })
                    .collect()
            });
            let first = scheduler::rendezvous_rank(SUCCESSOR_CONTEXT, &gang.0, live)
                .into_iter()
                .next();
            if first != Some(me) {
                continue;
            }
            info!(gang = %gang.short(), dead_owner = %owner.short(), committed,
                "adopting orphaned gang");
            if let Err(e) = self
                .inner
                .engine
                .author_op(OpBody::GangAdopt { gang, by: me })
            {
                warn!(gang = %gang.short(), "cannot record gang adoption: {e}");
                continue;
            }
            self.clone().spawn_gang_drive(plan, committed);
        }
    }

    pub(super) fn spawn_gang_drive(self, plan: mesh_proto::GangPlanRecord, committed: bool) {
        let gang = plan.gang;
        // Claimed here, before the task exists, so two adoption ticks
        // cannot both decide to drive it. The committed branch releases it
        // itself; the uncommitted branch hands it to `drive_gang`, which
        // holds it across the whole restart chain.
        if committed && !self.claim_gang(gang) {
            return;
        }
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                if committed {
                    // Already running: watch it out and enforce the
                    // member-death rule, but never re-offer — the members
                    // are journaled and re-dispatching them would be the
                    // one thing an all-or-nothing scheduler must not do.
                    let attempt = this
                        .watch_gang(&plan, None, &outputs::Wanted::default(), None)
                        .await;
                    this.close_gang(gang, attempt.outcome, 0);
                    this.release_gang(gang);
                } else {
                    // Never committed: the dead origin's leases expired on
                    // their own, so there is nothing to clean up and the
                    // offer phase simply runs again. `drive_gang` takes the
                    // claim; if another task beat us to it, it returns.
                    this.drive_gang(plan, None).await;
                }
            }
        });
        self.track(handle);
    }

    /// `meshnode gangs ls`.
    pub(super) fn gang_rows(&self, all: bool) -> Vec<mesh_proto::GangRow> {
        let me = self.node_id();
        let own_leases = self.inner.jobs.leases();
        self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::GangRow> = s
                .gangs
                .values()
                .filter(|e| all || e.closed.is_none())
                .map(|e| {
                    let owner = e.owner();
                    let committed = e.committed(&s.jobs);
                    let held = if committed {
                        u32::try_from(
                            e.plan
                                .members
                                .iter()
                                .filter(|m| s.jobs.contains_key(&m.job_id))
                                .count(),
                        )
                        .unwrap_or(u32::MAX)
                    } else {
                        // Only this node can say for certain, and only about
                        // itself: a lease does not replicate.
                        u32::from(own_leases.iter().any(|l| l.gang == e.plan.gang))
                    };
                    mesh_proto::GangRow {
                        gang: e.plan.gang,
                        ticket: e.plan.ticket,
                        preview: Self::gang_spec(&e.plan, 0).preview(),
                        size: e.plan.size(),
                        placement: e.plan.placement,
                        submitter_user: e.plan.submitter_user,
                        username: s
                            .username(&e.plan.submitter_user)
                            .map(str::to_string)
                            .unwrap_or_else(|| e.plan.submitter_user.to_display_string()),
                        owner,
                        owner_name: s
                            .members
                            .get(&owner)
                            .map(|m| m.info.name.clone())
                            .unwrap_or_else(|| owner.short()),
                        state: match (&e.closed, committed) {
                            (Some((outcome, _)), _) => outcome.as_str().to_string(),
                            (None, true) => "running".to_string(),
                            (None, false) => "offering".to_string(),
                        },
                        outcome: e.closed.map(|(o, _)| o),
                        held,
                        created_at_micros: e.plan.created_at_micros,
                    }
                })
                .collect();
            // Seniority order, so the table *is* the queue: "who is senior
            // to whom" is the question this noun exists to answer.
            rows.sort_by_key(|r| r.ticket);
            let _ = me;
            rows
        })
    }
}
