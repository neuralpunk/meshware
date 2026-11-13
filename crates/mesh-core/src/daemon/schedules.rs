//! Schedules: the rows a person reads, and the tick that fires one.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2). Another banner-marked
//! section — somebody had already decided it was one topic.

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    // ---------------------------------------------------------------------
    // N1 P5 — cron, standalone
    // ---------------------------------------------------------------------
    /// Rows for `schedule ls` / `schedule show` (N1 P5).
    pub(super) fn schedule_rows(
        &self,
        only: Option<&str>,
        with_source: bool,
    ) -> Vec<mesh_proto::ServiceScheduleRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            s.service_schedules
                .values()
                .filter(|e| !e.removed)
                .filter(|e| only.is_none_or(|name| e.schedule.name == name))
                .map(|e| {
                    let sc = &e.schedule;
                    // "Orphaned" is its own word on purpose: a schedule
                    // whose service was retired is not paused and not
                    // active, it is broken, and the difference is the
                    // whole reason a `schedule ls` is worth reading.
                    let status = if sc.paused {
                        "paused"
                    } else if !crate::schedules::validate(s, sc).is_empty() {
                        "orphaned"
                    } else {
                        "active"
                    };
                    let firer = crate::schedules::elected_firer(s, &sc.name, |id| {
                        self.inner.gossip.liveness_of(id)
                    })
                    .and_then(|id| s.members.get(&id).map(|m| m.info.name.clone()));
                    mesh_proto::ServiceScheduleRow {
                        name: sc.name.clone(),
                        service: sc.service_ref(),
                        targets: sc.targets.clone(),
                        when: sc.when.describe(),
                        status: status.to_string(),
                        next_micros: sc.when.next_after(now),
                        last_micros: crate::schedules::last_fired(
                            s,
                            sc,
                            now,
                            crate::schedules::LOOK_BACK,
                        )
                        .map(|(at, _)| at),
                        firer,
                        created_by_username: crate::rbac::username_of(s, &founder, &sc.created_by),
                        created_at_micros: sc.created_at_micros,
                        source: with_source.then(|| crate::schedules::to_toml(sc)),
                    }
                })
                .collect()
        })
    }

    /// Publish (or replace) a schedule, or report a dry run.
    pub(super) fn schedule_put(
        &self,
        user: UserId,
        env: &SignedIpc,
        schedule: mesh_proto::ServiceSchedule,
        dry_run: bool,
    ) -> IpcResponse {
        let now = mesh_proto::now_micros();
        let (issues, matching) = self.inner.engine.read(|s| {
            let issues: Vec<String> = crate::schedules::validate(s, &schedule)
                .into_iter()
                .map(|i| i.message)
                .collect();
            let selector = Selector::parse(&schedule.targets);
            let matching = membership::resolve_targets(s, &selector, self.node_id(), |id| {
                self.inner.gossip.liveness_of(id)
            })
            .len();
            (issues, u32::try_from(matching).unwrap_or(u32::MAX))
        });
        if dry_run {
            return IpcResponse::ScheduleCheck(Box::new(mesh_proto::ServiceScheduleCheck {
                name: schedule.name.clone(),
                service: schedule.service_ref(),
                targets: schedule.targets.clone(),
                when: schedule.when.describe(),
                matching_nodes: matching,
                next_micros: schedule.when.next_after(now),
                issues,
            }));
        }
        if !issues.is_empty() {
            return IpcResponse::Error {
                message: issues.join("; "),
            };
        }
        let name = schedule.name.clone();
        let replacing = self
            .inner
            .engine
            .read(|s| s.service_schedules.get(&name).is_some_and(|e| !e.removed));
        let paused = schedule.paused;
        let when = schedule.when.describe();
        match crate::schedules::put(&self.inner.engine, schedule) {
            Ok(()) => {
                let action = match (replacing, paused) {
                    (true, true) => "schedule.pause",
                    (true, false) => "schedule.put",
                    (false, _) => "schedule.create",
                };
                self.audit_ok(&user, action, &name, &when, env);
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        }
    }

    /// One pass over every active schedule this node is elected to fire.
    ///
    /// Due-ness is read from **replicated state, never from this node's
    /// memory**: "has 03:00 already fired?" is "does the job it would have
    /// created exist?", which every node answers identically, which survives
    /// a restart for free, and which is why two firers during churn
    /// converge on one job instead of running the backup twice.
    pub(super) async fn schedule_tick(&self) {
        // N6 §12, and on the same switch as deployment reconciliation
        // rather than a third one. Both are the *mesh* deciding to start
        // something without a person asking, which is exactly the class an
        // operator wants to stop while they stabilise — and a schedule that
        // kept firing through a shed would be the loudest possible
        // counter-example to the switch working.
        //
        // A schedule that does not fire records a skip with its reason, so
        // the gap is legible afterwards rather than looking like a mesh
        // that forgot. `catch_up` then decides whether the missed
        // occurrences run when the shed lifts, which is the decision N1 P5
        // already made and this does not second-guess.
        if self.shed().is_some_and(|s| s.reconcile) {
            return;
        }
        let me = self.node_id();
        let now = mesh_proto::now_micros();
        let founder = self.inner.engine.genesis().body.founder_user;

        // Decide under one read; act outside it. A fire authors a job and
        // may cross the network, and neither belongs inside the state lock.
        let decisions: Vec<(mesh_proto::ServiceSchedule, crate::schedules::Due)> =
            self.inner.engine.read(|s| {
                s.service_schedules
                    .values()
                    .filter(|e| e.is_active())
                    .filter(|e| {
                        crate::schedules::creator_may_still_run(s, &founder, &e.schedule, now)
                    })
                    .filter(|e| {
                        crate::schedules::elected_firer(s, &e.schedule.name, |id| {
                            self.inner.gossip.liveness_of(id)
                        }) == Some(me)
                    })
                    .map(|e| {
                        let sched = e.schedule.clone();
                        let last = crate::schedules::last_fired(
                            s,
                            &sched,
                            now,
                            crate::schedules::LOOK_BACK,
                        )
                        .map(|(at, _)| at);
                        let due = crate::schedules::due(
                            &sched,
                            now,
                            last,
                            |id| s.jobs.contains_key(&id),
                            || {
                                crate::schedules::running_job(
                                    s,
                                    &sched,
                                    now,
                                    crate::schedules::LOOK_BACK,
                                )
                            },
                        );
                        (sched, due)
                    })
                    .filter(|(_, due)| !matches!(due, crate::schedules::Due::Idle))
                    .collect()
            });

        for (schedule, due) in decisions {
            match due {
                crate::schedules::Due::Idle => {}
                crate::schedules::Due::Fire { occurrence_micros } => {
                    self.fire_schedule(&schedule, occurrence_micros).await;
                }
                crate::schedules::Due::Skipped {
                    occurrence_micros,
                    skip,
                } => {
                    self.record_schedule_skip(&schedule, occurrence_micros, &skip);
                }
            }
        }
    }
}
