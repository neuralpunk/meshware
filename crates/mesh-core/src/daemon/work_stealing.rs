//! `meshnode parallel` and `meshnode map`: work stealing across the mesh,
//! and the owner's view of it.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    pub(super) async fn parallel_stream(
        &self,
        req: ParallelRequest,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let ParallelRequest {
            user,
            auth,
            command,
            exec,
            item_input,
            items,
            on,
            mode,
            env: job_env,
            timeout_secs,
            chunk,
            capture,
            reduce,
            reduce_on,
        } = req;
        if items.is_empty() {
            let _ = out
                .send(IpcResponse::Error {
                    message: "nothing to do: the item list is empty".into(),
                })
                .await;
            return;
        }
        let me = self.node_id();
        let selector = Selector::parse(on.as_deref().unwrap_or("any"));

        // The request, verbatim, before anything below rewrites the
        // environment — what a `--resume` of this run will resubmit.
        let request_env = job_env.clone();
        let request_items = items.clone();
        let (request_command, request_service, request_inputs, request_item_input) = (
            command.clone(),
            exec.as_ref().map(|e| e.service_name.clone()),
            exec.as_ref().map(|e| e.inputs.clone()).unwrap_or_default(),
            item_input.clone(),
        );
        let (request_reduce, request_reduce_on) = (reduce.clone(), reduce_on.clone());
        // `--halt-on-fail` and `--resume` are the owner's business and
        // never a worker's: off the environment here, before the plan is
        // built, the cache key derived, or the gate judges the plan.
        let mut job_env = job_env;
        let halt = job_env
            .iter()
            .find(|(k, _)| k == mesh_proto::parallel::REQ_HALT_ON_FAIL)
            .and_then(|(_, v)| mesh_proto::HaltPolicy::parse(v));
        let resumes = job_env
            .iter()
            .find(|(k, _)| k == mesh_proto::parallel::REQ_RESUMES)
            .and_then(|(_, v)| mesh_proto::JobId::parse(v).ok());
        job_env.retain(|(k, _)| {
            k != mesh_proto::parallel::REQ_HALT_ON_FAIL && k != mesh_proto::parallel::REQ_RESUMES
        });

        // `--json` / `--outputs` are the origin's business and never the
        // worker's, so they come off here (M19 §5.2). `--expect` does not
        // apply to `parallel`: an item is not a node, and asserting over
        // items is what `--json | jq` is for.
        let (want, _) = outputs::Wanted::take_from_env(&mut job_env);
        let job_env = job_env;

        let reqs = mesh_proto::Requirements::from_env(&job_env);
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        let pressure_of = self.pressure_lookup();

        // Workers are the nodes that could run one item of this job: the
        // same feasibility and grant rules as an ordinary run (§5.3, §7.2).
        let service_name = exec.as_ref().map(|e| e.service_name.clone());
        // T3. A `map` over a published service runs that service's runtime
        // on every worker; a `map` over an inline body is a shell script.
        // Either way a worker that cannot run it is not a worker.
        let worker_runtime = exec
            .as_ref()
            .map(|e| e.runtime)
            .unwrap_or(mesh_proto::Runtime::Bash);
        let placement_env = mesh_proto::runtimes::env_for_placement(&job_env, worker_runtime);
        let workers: Vec<MemberInfo> = self.inner.engine.read(|s| {
            membership::resolve_feasible_for_env(
                s,
                &selector,
                me,
                |id| self.inner.gossip.liveness_of(id),
                &placement_env,
                &pressure_of,
            )
            .into_iter()
            .filter(|m| rbac::can_run_on(s, &founder, &user, m, service_name.as_deref(), now))
            .collect()
        });
        // Can these machines see the data? (R4 P2)
        //
        // Work stealing means any worker may end up with any item, so a node
        // that cannot open the library is a node that will fail whichever
        // item it happens to pull. Asked once per candidate about the items'
        // common directory, not once per item: a mount is present or absent.
        // Generated here rather than at plan construction: staged input
        // objects are owned by this job, and they are ingested before the
        // plan exists (R4 P3).
        let job_id = JobId::generate();
        // Kept so staging can hand every candidate back: once the bytes are
        // going to travel, none of them needs to see the path.
        let workers_before_reach = workers.clone();
        // What this run needs of a machine, from the service's own
        // declaration: the data, and the commands.
        let declared_tools: Vec<String> = exec
            .as_ref()
            .and_then(|e| {
                self.inner.engine.read(|s| {
                    s.services
                        .get(&e.service_name)
                        .and_then(|entry| entry.versions.get(&e.version))
                        .and_then(|v| v.contract.clone())
                })
            })
            .map(|c| crate::script_spec::ScriptSpec::from_contract(&c).tools)
            .unwrap_or_default();
        // Where the results are supposed to land counts too (R4 P4). A
        // service declaring `writes $outdir` and a worker that cannot see
        // that directory is the same failure as an unreadable input, found
        // at the end of the work instead of before it — and the end of the
        // work is where it is most expensive.
        let declared_writes: Vec<String> = exec
            .as_ref()
            .map(|e| {
                let contract = self.inner.engine.read(|s| {
                    s.services
                        .get(&e.service_name)
                        .and_then(|entry| entry.versions.get(&e.version))
                        .and_then(|v| v.contract.clone())
                });
                contract
                    .map(|c| {
                        let d = crate::script_spec::ScriptSpec::from_contract(&c);
                        crate::script_spec::resolve_paths(&d.writes, &e.inputs)
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let (mut workers, mut unreachable) = self
            .filter_for_reach(workers, &items, &declared_writes, &declared_tools)
            .await;
        // Nobody can see the data — so send it to them (R4 P3).
        //
        // The bytes go into the content store here and are materialised into
        // each worker's own chunk directory, which is the thing that makes a
        // mesh with no shared filesystem work at all. Bounded, because
        // staging is right for a folder of videos and wrong for a dataset:
        // over `[compute] stage_max_mb` the run is refused with both fixes
        // named rather than quietly spending an hour of somebody's uplink.
        let mut staged: Vec<String> = Vec::new();
        let mut staged_ids: Vec<mesh_proto::ObjectId> = Vec::new();
        if workers.is_empty() && !unreachable.is_empty() {
            let ceiling = self
                .inner
                .config
                .compute
                .stage_max_mb
                .saturating_mul(1024 * 1024);
            let files: Vec<&String> = items
                .iter()
                .filter(|i| std::path::Path::new(i.trim()).is_absolute())
                .collect();
            let total: u64 = files
                .iter()
                .filter_map(|f| std::fs::metadata(f.trim()).ok())
                .map(|m| m.len())
                .sum();
            if ceiling > 0 && !files.is_empty() && total <= ceiling {
                let me = self.node_id();
                let mut ok = true;
                for item in &items {
                    let path = std::path::Path::new(item.trim());
                    if !path.is_absolute() {
                        staged.push(String::new());
                        continue;
                    }
                    match self.inner.objects.put_file_off_thread(job_id, path).await {
                        Ok(mut obj) => {
                            obj.holder = me;
                            staged.push(obj.id.to_display_string());
                        }
                        Err(e) => {
                            warn!("cannot stage {}: {e}", path.display());
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    // Tell the directory who holds them, and pin them for
                    // the run. Without the announcement a worker asks the
                    // mesh where the bytes are and is told nowhere — the
                    // object exists here and nobody else can find it. The
                    // pin is because these are inputs to a job that has not
                    // started: the ordinary TTL is measured from *now*, and
                    // a long queue would expire them before they are read.
                    let announce: Vec<(mesh_proto::ObjectId, u64)> = staged
                        .iter()
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| mesh_proto::ObjectId::parse(s).ok())
                        .filter_map(|id| self.inner.objects.manifest(&id).map(|m| (id, m.size)))
                        .collect();
                    let ids: Vec<mesh_proto::ObjectId> =
                        announce.iter().map(|(id, _)| *id).collect();
                    // Staged inputs have a real owner that releases them
                    // when the run ends (`unpin`, below). The lease is only
                    // the backstop for a run that dies without doing so, so
                    // it is generous — a parallel run may legitimately take
                    // a day.
                    self.inner.objects.pin(&ids, STAGED_INPUT_LEASE);
                    staged_ids = ids.clone();
                    self.announce_objects(job_id, &announce).await;
                    // Every candidate is a candidate again: none of them
                    // needs to see the path any more.
                    workers = workers_before_reach;
                    unreachable.clear();
                    info!(
                        items = ids.len(),
                        bytes = total,
                        "staging inputs to workers that cannot see them"
                    );
                } else {
                    staged.clear();
                }
            } else if ceiling > 0 && total > ceiling {
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "no node matching {selector:?} can see this data, and it is too \
                             large to send: {} MB against a [compute] stage_max_mb of {} MB. \
                             Mount it on the machines that should run this, or raise the \
                             ceiling.",
                            total / (1024 * 1024),
                            self.inner.config.compute.stage_max_mb
                        ),
                    })
                    .await;
                return;
            }
        }
        if workers.is_empty() && !unreachable.is_empty() {
            // Every candidate answered "no". A different problem from
            // "nobody matched", with a different fix — so it says which of
            // the two it was, and names the thing that is missing rather
            // than the state it produced.
            let data = crate::reach::common_ancestor(&items)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let missing_tool = unreachable
                .iter()
                .find_map(|d| d.split(": ").nth(1))
                .filter(|r| r.contains("not installed"))
                .map(|r| r.trim_end_matches(" is not installed").to_string());
            let message = match (missing_tool, data.is_empty()) {
                (Some(tool), _) => format!(
                    "no node matching {selector:?} has {tool} installed — every candidate was \
                     asked. Install it on the machines that should run this, or narrow the \
                     selector to the ones that already have it.\n  {}",
                    unreachable.join("\n  ")
                ),
                (None, false) => format!(
                    "no node matching {selector:?} can see {data} — every candidate was asked \
                     and none of them has it. Mount it on the machines that should run this, \
                     or narrow the selector to the ones that already have it."
                ),
                (None, true) => format!(
                    "no node matching {selector:?} can run this:\n  {}",
                    unreachable.join("\n  ")
                ),
            };
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        }
        // Ran, but on fewer machines than the selector matched. Reported
        // through the channel that already exists for exactly this — the
        // `name: reason` list that stops a one-node run looking like a
        // broken mesh — so it reaches the CLI and Director without either
        // learning a new frame.

        if workers.is_empty() {
            // Same distinction `run` makes: "nobody matched" and "nobody is
            // big enough" are different problems, and only one of them is
            // about the selector.
            let matched = self.inner.engine.read(|s| {
                membership::resolve_targets(s, &selector, me, |id| {
                    self.inner.gossip.liveness_of(id)
                })
                .len()
            });
            let message = if matched > 0 && !reqs.is_unstated() {
                format!(
                    "no live node matching {selector:?} has room for {} per item",
                    describe_requirements(&reqs)
                )
            } else {
                format!("no live node matching {selector:?} can run this")
            };
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        }

        // A parallel job's preview names the workers that could take a
        // chunk: work stealing means any of them may end up running
        // something, so that is the honest list of what it touches.
        if mode.is_dry_run() {
            let payload = match &exec {
                Some(e) => JobPayload::Service(e.clone()),
                None => JobPayload::Command(command.clone()),
            };
            let resolved = Resolved {
                action: match &exec {
                    Some(e) => format!("{}@{} × {} items", e.service_name, e.version, items.len()),
                    None => format!("{} × {} items", command.replace('\n', " "), items.len()),
                },
                selector: on.clone().unwrap_or_else(|| "any".into()),
                targets: &workers,
                priority: JobPriority::Normal,
            };
            let mut preview = job_preview(&resolved, &payload, &job_env, timeout_secs, &[]);
            preview.kind = "parallel".into();
            let _ = out.send(IpcResponse::JobPreview(Box::new(preview))).await;
            return;
        }

        let total = items.len() as u64;
        // The staged manifest rides the environment, for the reason
        // `capacity.rs` gives for every submission-side directive: a plan
        // crosses the peer wire and postcard encodes positionally, so a new
        // field would stop a node one version behind decoding *any* plan
        // from this origin (R4 P3).
        let mut job_env = job_env;
        if staged.iter().any(|s| !s.is_empty()) {
            job_env.push((mesh_proto::STAGED_ITEMS.to_string(), staged.join(",")));
        }
        let plan = Arc::new(mesh_proto::ParallelPlan {
            job: job_id,
            command,
            exec,
            item_input,
            items,
            env: job_env,
            timeout_secs,
            chunk,
            capture,
            inputs: Vec::new(),
            workers: workers.iter().map(|m| m.node_id).collect(),
            submitter_user: user,
            auth: auth.clone(),
        });
        let (job, mut results) = parallel::ParallelJob::own_with(plan.clone(), self.straggler());
        self.inner.parallel.insert(job.clone());
        self.inner
            .groups
            .started(crate::observe::GroupKind::Parallel);
        tracing::Span::current().record("job", tracing::field::display(job_id.short()));
        let started = std::time::Instant::now();
        let created = mesh_proto::now_micros();
        self.publish_parallel(&job, JobState::Running, created, None);
        // The record, request half: written now so an owner that dies
        // mid-run still leaves behind what the run *was*.
        let mut record = mesh_proto::parallel::ParallelRecord {
            job: job_id,
            owner: me,
            owner_name: self.inner.config.node_name.clone(),
            submitter_user: user,
            created_at_micros: created,
            finished_at_micros: None,
            command: request_command,
            service: request_service,
            inputs: request_inputs,
            item_input: request_item_input,
            items: request_items,
            on: on.clone(),
            env: request_env,
            timeout_secs,
            chunk,
            capture,
            reduce: request_reduce,
            reduce_on: request_reduce_on,
            resumes,
            halted: None,
            outcomes: Vec::new(),
        };
        self.remember_parallel(&record);
        let halt_at =
            halt.map(|h| u64::from(h.threshold(u32::try_from(total).unwrap_or(u32::MAX))));
        audit::record(
            &self.inner.engine,
            user,
            "job.parallel",
            &job_id.to_display_string(),
            "ok",
            &format!("{total} items over {} node(s)", workers.len()),
            &auth.signature,
        );
        let dispatch = self.start_lanes(&job, &workers).await;
        info!(job = %job_id.short(), lanes = dispatch.lanes,
            nodes = dispatch.workers.len(), "parallel job dispatched");
        let _ = out
            .send(IpcResponse::ParallelStarted {
                job_id,
                total,
                workers: dispatch.workers,
                declined: dispatch
                    .declined
                    .into_iter()
                    .chain(unreachable.iter().cloned())
                    .collect(),
                // How long this is likely to take (R5 P4). Per-item cost
                // from what this service has actually done before, times the
                // items, divided by the lanes that will run them — the same
                // history `meshnode plan` reads, so the number before you
                // press enter and the number `plan` gives you cannot
                // disagree.
                estimate: self
                    .historic_duration(service_name.as_deref(), &workers)
                    .map(|d| {
                        let lanes = dispatch.lanes.max(1) as u64;
                        let scale = |per_item: u64| {
                            per_item.saturating_mul(total).saturating_add(lanes - 1) / lanes
                        };
                        mesh_proto::DurationRange {
                            p10_secs: scale(d.p10_secs),
                            p50_secs: scale(d.p50_secs),
                            p90_secs: scale(d.p90_secs),
                            samples: d.samples,
                        }
                    }),
            })
            .await;

        // Stream: items as they finish, progress on a tick, done at the end.
        // Map outputs accumulate as references — never as bytes, which is
        // the whole point — so a reduce has something to be handed.
        let mut produced: Vec<mesh_proto::ObjectRef> = Vec::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(400));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                got = results.recv() => match got {
                    Some(result) => {
                        if !self.emit_items(&out, result, &mut produced, &want, &mut record.outcomes).await {
                            job.cancel();
                            break;
                        }
                        // `--halt-on-fail`: the circuit breaker. Stop
                        // handing out work the moment the failure count
                        // reaches the threshold; what is in flight
                        // finishes and reports, and the rest is untouched
                        // — which is what a `--resume` picks up.
                        if let Some(at) = halt_at {
                            let failed = job.progress().failed;
                            if failed >= at {
                                let pct = (failed as f64 / total as f64) * 100.0;
                                record.halted = Some(format!(
                                    "--halt-on-fail {} tripped at {failed} failure(s) of {total} \
                                     ({pct:.0}%); nothing more was handed out",
                                    halt.map(|h| h.to_string()).unwrap_or_default()
                                ));
                                job.cancel();
                                break;
                            }
                        }
                    }
                    None => break,
                },
                _ = ticker.tick() => {
                    self.reap_dead_workers(&job);
                    let p = job.progress();
                    let sent = out
                        .send(IpcResponse::ParallelProgress {
                            total,
                            done: p.done,
                            failed: p.failed,
                            inflight: p.inflight,
                            chunk_items: p.chunk_items,
                            speculated: p.speculated,
                            per_node: p.per_node,
                        })
                        .await;
                    if sent.is_err() {
                        job.cancel();
                        break;
                    }
                    if job.finished() {
                        break;
                    }
                }
            }
        }
        // Anything already reported but not yet streamed.
        while let Ok(result) = results.try_recv() {
            if !self
                .emit_items(&out, result, &mut produced, &want, &mut record.outcomes)
                .await
            {
                break;
            }
        }
        let p = job.progress();
        let (done, failed, per_node) = (p.done, p.failed, p.per_node);
        let mut state = if failed > 0 || record.halted.is_some() {
            JobState::Failed
        } else {
            JobState::Succeeded
        };
        // The reduce runs after every item, on whichever node already holds
        // the most of the output — which is §5.5's pre-reduce rule, and the
        // reason the map's bytes never had to move at all.
        let had_reduce = reduce.is_some();
        if let Some(reduce) = reduce
            && failed == 0
        {
            let ok = self
                .reduce_stream(&plan, reduce, reduce_on, &produced, &out)
                .await;
            if !ok {
                state = JobState::Failed;
            }
        }
        // A finished run's output is the answer, not an intermediate, so
        // it stops being on the fifteen-minute clock until somebody
        // collects it (R3 P3). Only what is still *here* — a result whose
        // only copy lives on another node is that node's to hold.
        //
        // The reduce case is deliberately excluded: its inputs were
        // consumed to make the answer, and holding both is holding the same
        // information twice.
        if !produced.is_empty() && !had_reduce {
            let me = self.node_id();
            let mine: Vec<mesh_proto::ObjectId> = produced
                .iter()
                .filter(|o| o.holder == me)
                .map(|o| o.id)
                .collect();
            if !mine.is_empty() {
                let ceiling = self
                    .inner
                    .config
                    .compute
                    .uncollected_max_mb
                    .saturating_mul(1024 * 1024);
                let evicted = self.inner.objects.retain_results(job_id, &mine, ceiling);
                for old in &evicted {
                    warn!(
                        job = %old.short(),
                        "uncollected results released to the ordinary TTL: this node is holding \
                         more than [compute] uncollected_max_mb. Fetch them sooner, or raise it."
                    );
                }
            }
        }
        // Staged inputs were pinned for the run's duration; the run is
        // over.
        if !staged_ids.is_empty() {
            self.inner.objects.unpin(&staged_ids);
        }
        let finished = mesh_proto::now_micros();
        self.publish_parallel(&job, state, created, Some(finished));
        self.inner.parallel.remove(&job_id);
        record.finished_at_micros = Some(finished);
        self.remember_parallel(&record);
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let succeeded = done.saturating_sub(failed);
        self.inner.groups.finished(
            crate::observe::GroupKind::Parallel,
            if record.halted.is_some() {
                crate::observe::GroupOutcome::Halted
            } else if failed > 0 {
                crate::observe::GroupOutcome::Failed
            } else {
                crate::observe::GroupOutcome::Completed
            },
            started.elapsed(),
            succeeded,
            failed,
            // Items the run never reached: a halted run leaves them, and
            // they became a chunk on no worker, so nothing else counts them.
            total.saturating_sub(done),
        );
        let _ = out
            .send(IpcResponse::ParallelDone {
                total,
                succeeded,
                failed,
                micros,
                speculated: p.speculated,
                per_node,
                halted: record.halted.clone(),
            })
            .await;
    }
}
