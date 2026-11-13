//! Sending one submission somewhere and streaming it back: the single
//! job, the fan-out, and the sub-job each target runs.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// Shared dispatch: filter targets by selector + grants, rank them with
    /// the scoring scheduler (MASTERPLAN §10.1), evaluate change control
    /// (M11 §5), submit, audit, and stream output back. `auth` is the
    /// user's signed request envelope — it travels inside the spec so the
    /// executor can re-verify (§7.2). With `fanout` set, the best matches
    /// become the targets of one logical job (§9.3) and the stream
    /// switches to the Fan* framing.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_and_stream(
        &self,
        user: UserId,
        auth: SignedIpc,
        payload: JobPayload,
        service: Option<String>,
        on: Option<String>,
        env: Vec<(String, String)>,
        timeout_secs: Option<u64>,
        priority: JobPriority,
        fanout: Option<u32>,
        controls: FanoutControls,
        mode: SubmitMode,
        out: mpsc::Sender<IpcResponse>,
    ) {
        // N6 §12 — the mesh-wide shed, checked before anything is resolved,
        // hashed, gated or placed.
        //
        // At the top for the same reason the meter below it starts at the
        // top: this is where a submission *arrives*, and a shed that
        // refused after change control had already recorded an approval
        // would leave an audit trail for work that never ran.
        if let Some(shed) = self.shed()
            && shed.submissions
        {
            let _ = out
                .send(IpcResponse::Error {
                    message: shed.refusal(),
                })
                .await;
            return;
        }

        // N6 §12 — the meter starts here, at the top of the handler, so what
        // it reports is everything meshware does to a submission: selector,
        // feasibility, grants, ranking, change control, the cache lookup,
        // and the round trip to whichever node takes it, spills included.
        // Starting it after the ranking would have measured the cheap half
        // and reported it as the whole.
        let dispatch_began = std::time::Instant::now();
        let selector = Selector::parse(on.as_deref().unwrap_or("any"));
        let me = self.node_id();
        let founder = self.inner.engine.genesis().body.founder_user;

        // `--hosts` / `--inventory` become the full reserved environment
        // here, on the origin, before anything is judged or placed: change
        // control hashes the resolved plan, the audit log records it, and
        // placement is chosen for it (M12 §6.2).
        let mut env = env;
        // `--singleton NAME` is the origin's to enforce and nobody else's
        // to see: off the environment before the cache key, the gate and
        // the spec. Checked below, once placement has produced a job id
        // to claim the name for.
        let singleton = env
            .iter()
            .find(|(k, _)| k == mesh_proto::REQ_SINGLETON)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty());
        env.retain(|(k, _)| k != mesh_proto::REQ_SINGLETON);
        if let Some(name) = &singleton {
            // A gang or a fan-out is N jobs, and a claim names one. Refused
            // rather than claiming the first: a name that read as held
            // while one of six hundred targets was still going would be a
            // rule nobody could predict.
            if fanout.is_some() || mesh_proto::gang_size_of(&env).is_some() {
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "--singleton {name:?} applies to a single run; a fan-out or a gang \
                             is several jobs, and a name can only be held by one"
                        ),
                    })
                    .await;
                return;
            }
            let now = mesh_proto::now_micros();
            let held = self.inner.engine.read(|s| {
                s.singleton_holder(name, now).map(|h| {
                    (
                        h.job,
                        self.member_name(&h.holder),
                        s.jobs.get(&h.job).map(|j| self.member_name(&j.executor)),
                        h.at_micros,
                    )
                })
            });
            if let Some((job, origin, executor, since)) = held {
                let ago = now.saturating_sub(since) / 1_000_000;
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "singleton {name:?} is held by {} (submitted from {origin}{} {}s ago) \
                             — it is still running, and a second copy was refused",
                            job.short(),
                            executor
                                .map(|e| format!(", running on {e}"))
                                .unwrap_or_default(),
                            ago
                        ),
                    })
                    .await;
                return;
            }
        }
        // Result capture is decided here and nowhere else (M19 §5.2): the
        // keys come off before change control hashes the plan, before
        // placement runs and before any spec is built, so an executor sees
        // exactly the environment it saw before M19 existed.
        let (want, expect) = outputs::Wanted::take_from_env(&mut env);
        let expect = match expect.as_deref().map(crate::results::Expectation::parse) {
            Some(Ok(e)) => Some(e),
            // A guard that fails to parse must never read as a pass, so
            // this refuses the submission rather than dropping the
            // assertion and running anyway.
            Some(Err(message)) => {
                let _ = out.send(IpcResponse::Error { message }).await;
                return;
            }
            None => None,
        };
        // Declared resource requirements ride the job environment (M9 §5.3).
        let reqs = mesh_proto::Requirements::from_env(&env);

        // -- C7 the action key (§11.2) -----------------------------------
        //
        // Derived here, on the origin, before placement: the origin is the
        // node that will do the lookup, and a key computed on two nodes
        // from two views of the same job is a key that silently never hits.
        //
        // Before placement because a hit needs no placement at all.
        let action = if mesh_proto::is_cacheable(&env) {
            let gang = mesh_proto::gang_size_of(&env).is_some();
            if gang || fanout.is_some() {
                // A fan-out is N *different* actions: each one ran on a
                // different machine and observed a different machine, so a
                // single key standing for all of them would hand one node's
                // answer to five hundred. Refused rather than silently
                // ignored, because a `--cacheable` that quietly did nothing
                // is worse than one that says why.
                //
                // The per-item win the milestone actually promises lives in
                // `map`, where each item genuinely is its own action.
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "--cacheable does not apply to a {}: each target runs on a different machine and is its own action. Use `meshnode map` for per-item caching.",
                            if gang { "gang" } else { "fan-out" }
                        ),
                    })
                    .await;
                return;
            }
            match crate::cache::derive_from(
                &payload,
                &env,
                self.inner.actions.epoch(),
                &crate::cache::arch(),
            ) {
                Ok(action) => Some(action),
                Err(message) => {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: message.to_string(),
                        })
                        .await;
                    return;
                }
            }
        } else {
            None
        };
        if mode.is_explain_key() {
            let response = match &action {
                Some(action) => IpcResponse::CacheKey(Box::new(action.explain())),
                None => IpcResponse::Error {
                    message: concat!(
                        "--explain-key needs --cacheable: an action key is only meaningful ",
                        "for work whose author has claimed it is a pure function of its inputs",
                    )
                    .into(),
                },
            };
            let _ = out.send(response).await;
            return;
        }
        let env = env;
        // T3: the runtime this payload needs, folded into the environment
        // the filter reads. A container job must never be dispatched to a
        // node with no engine, nor a python job to one with no python3.
        let placement_env = mesh_proto::runtimes::env_for_placement(&env, payload.needs_runtime());

        let pressure_of = self.pressure_lookup();
        let targets = self.inner.engine.read(|s| {
            membership::resolve_feasible_for_env(
                s,
                &selector,
                me,
                |id| self.inner.gossip.liveness_of(id),
                &placement_env,
                &pressure_of,
            )
        });
        // 1.15: a script job's envelope is undecodable to an older peer
        // (`mesh_proto::script`). Filtered here rather than at submit time
        // so a mixed mesh *places* it on a node that can run it, instead of
        // discovering the problem one dispatch at a time.
        let targets = if mesh_proto::script::requires_script_support(&env) {
            match self.narrow_to_script_capable(targets) {
                Ok(targets) => targets,
                Err(message) => {
                    let _ = out.send(IpcResponse::Error { message }).await;
                    return;
                }
            }
        } else {
            targets
        };
        if targets.is_empty() {
            // Distinguish "nobody matched" from "nobody is big enough" —
            // the second is a sizing problem and the operator needs to know
            // which one they have.
            let matched = self.inner.engine.read(|s| {
                membership::resolve_targets(s, &selector, me, |id| {
                    self.inner.gossip.liveness_of(id)
                })
                .len()
            });
            let devices = mesh_proto::DeviceRequest::from_env(&env);
            let message = if matched > 0
                && let Some(req) = &devices
            {
                // Named separately from the scalar shortfall, because they
                // are different problems with different fixes: "no room"
                // means wait or resize, "no such device" means the mesh
                // does not contain the hardware and never will until
                // somebody buys some.
                format!(
                    "no live node matching {selector:?} advertises {}",
                    req.describe()
                )
            } else if matched > 0
                && let volumes = mesh_proto::VolumeRequests::from_env(&env)
                && !volumes.is_empty()
            {
                // Its own rung, above the scalar one, for the same reason
                // devices have one: "no room" is a sizing problem and "no
                // node lends `data`" is a `[[volume]]` block somebody has
                // not written. Saying the second as the first sends the
                // reader to look at load.
                format!(
                    "no live node matching {selector:?} lends {}",
                    volumes.names().join(", ")
                )
            } else if matched > 0
                && let ports = mesh_proto::PortRequests::from_env(&env)
                && !ports.is_empty()
            {
                format!(
                    "no live node matching {selector:?} can give this job a host port — \
                     every candidate advertises no [network] dynamic_ports, or already \
                     holds one it asked for"
                )
            } else if matched > 0 && !reqs.is_unstated() {
                format!(
                    "no live node matching {selector:?} has room for {}",
                    describe_requirements(&reqs)
                )
            } else {
                format!("no live node matches selector {selector:?}")
            };
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        }
        // Scoped grants: of the live matches, keep those this user may run
        // on (MASTERPLAN §7.2 selectors), then rank what's left (§10.1).
        let now = mesh_proto::now_micros();
        let bundle = match &payload {
            JobPayload::Service(exec) => Some(exec.bundle_id),
            JobPayload::Command(_) | JobPayload::Script(_) => None,
        };
        // Five GPU jobs should distribute by real GPU idleness, not by job
        // count (C5 P5). The figure is the *least busy matching device* on
        // each node — the card the job would actually get — and it is
        // `None` for every job that asked for no devices, so ordinary work
        // scores exactly as it did before this existed.
        // Kept past the point the environment is moved into the job spec:
        // the refusal at the bottom of this function needs to name what was
        // asked for, and by then the env belongs to the spec.
        let device_req = mesh_proto::DeviceRequest::from_env(&env);
        let device_busy_of = |id: &NodeId| self.device_busy(id, device_req.as_ref());
        // C8 P1: the same ranking, with the arithmetic kept instead of
        // discarded. `rank_explained` *is* `rank_with_devices` — one sorts
        // and returns, the other sorts, returns, and hands back the top
        // five breakdowns — so turning the explanation on cannot move a
        // job. The `chosen` field stays empty until a node has actually
        // taken the work, because a best candidate can answer `Busy` and
        // the job can spill.
        let (ranked, mut explain): (Vec<MemberInfo>, mesh_proto::PlacementExplain) =
            self.inner.engine.read(|s| {
                let allowed: Vec<MemberInfo> = targets
                    .into_iter()
                    .filter(|m| rbac::can_run_on(s, &founder, &user, m, service.as_deref(), now))
                    .collect();
                scheduler::rank_explained(
                    s,
                    allowed,
                    bundle.as_ref(),
                    me,
                    |id| self.inner.transport.rtt_micros(id),
                    &pressure_of,
                    |_| None,
                    device_busy_of,
                    self.placement(),
                )
            });
        if ranked.is_empty() {
            let message = "your grants do not cover any matching node".to_string();
            audit::record(
                &self.inner.engine,
                user,
                "job.run",
                service.as_deref().unwrap_or("(ad-hoc)"),
                &format!("denied: {message}"),
                "",
                &auth.signature,
            );
            let _ = out.send(IpcResponse::Denied { message }).await;
            return;
        }

        // What a preview says this touches: the nodes placement resolved
        // to, not the selector. A gang's is the **set it will hold**, which
        // is neither one node nor the whole selector (C4 §8), so it is
        // computed from exactly the inputs `dispatch_gang` will use.
        let gang_size = mesh_proto::gang_size_of(&env);
        if mode.is_dry_run() {
            let touched: Vec<MemberInfo> = match (gang_size, fanout) {
                (Some(size), _) => {
                    let placement = env
                        .iter()
                        .find(|(k, _)| k == mesh_proto::REQ_GANG_PLACEMENT)
                        .and_then(|(_, v)| mesh_proto::GangPlacement::parse(v))
                        .unwrap_or_default();
                    // Deduplicated: `pack` puts several members on one node.
                    let mut seen: Vec<MemberInfo> = Vec::new();
                    for m in Self::assign_gang(&ranked, size, &reqs, placement) {
                        if !seen.iter().any(|s| s.node_id == m.node_id) {
                            seen.push(m);
                        }
                    }
                    seen
                }
                (None, Some(0)) => ranked.clone(),
                (None, Some(limit)) => ranked.iter().take(limit as usize).cloned().collect(),
                // A single run spills to the next candidate if the first
                // is busy, so this is where it would land first.
                (None, None) => ranked.iter().take(1).cloned().collect(),
            };
            let resolved = Resolved {
                action: payload_preview(&payload),
                selector: on.clone().unwrap_or_else(|| "any".into()),
                targets: &touched,
                priority,
            };
            let excluded = self.excluded_names(&selector, &touched, &user, service.as_deref(), now);
            let preview = job_preview(&resolved, &payload, &env, timeout_secs, &excluded);
            let _ = out.send(IpcResponse::JobPreview(Box::new(preview))).await;
            return;
        }

        // -- C4 gangs (§8) -----------------------------------------------
        //
        // Before dispatch, because everything below assumes work can start
        // one target at a time and a gang's whole claim is that it does not.
        if let Some(size) = gang_size {
            self.dispatch_gang(
                user,
                auth,
                payload,
                env,
                timeout_secs,
                priority,
                size,
                ranked,
                want,
                expect,
                out,
            )
            .await;
            return;
        }

        if let Some(limit) = fanout {
            let mut targets = ranked;
            if limit > 0 {
                targets.truncate(limit as usize);
            }
            self.dispatch_fanout(
                user,
                auth,
                payload,
                env,
                timeout_secs,
                priority,
                controls,
                targets,
                want,
                expect,
                out,
            )
            .await;
            return;
        }

        // -- C7 the lookup (§11.3 P2) -------------------------------------
        //
        // Here, after change control and after placement, and before a
        // single byte is dispatched. After the gate because a hit that
        // bypassed governance would be a bypass; after placement because
        // the gate needs the blast radius; before dispatch because the
        // whole promise is that the second run does not run.
        let job_id = mesh_proto::JobId::generate();
        // The claim, once there is a job to claim for and after the gate:
        // a name claimed for a submission change control then refused
        // would hold for ten minutes over nothing.
        if let Some(name) = &singleton
            && let Err(e) = self.inner.engine.author_op(OpBody::SingletonClaim {
                name: name.clone(),
                job: job_id,
                user,
                at_micros: mesh_proto::now_micros(),
            })
        {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("cannot claim singleton {name:?}: {e}"),
                })
                .await;
            return;
        }
        let mut env = env;
        let mut verifying: Option<mesh_proto::CacheEntry> = None;
        let mut marker: Option<crate::history::JobCacheMarker> = None;
        if let Some(action) = &action {
            let key = action.key();
            marker = Some(crate::history::JobCacheMarker {
                key,
                hit: None,
                fields: action.explain().fields,
            });
            match self.cache_lookup(&key).await {
                Some(entry) if self.inner.actions.should_verify() => {
                    // `[cache] verify_rate`: run it anyway and check the
                    // answer (§11.3 P4). The job dispatches normally — the
                    // user gets a real execution — and the comparison
                    // happens as the stream finishes.
                    info!(
                        job = %job_id.short(),
                        key = %key.short(),
                        "spot-checking a cache hit"
                    );
                    verifying = Some(entry);
                    env.push((mesh_proto::CACHE_VERIFY.to_string(), "1".into()));
                }
                Some(entry) => {
                    self.serve_cache_hit(
                        job_id,
                        entry,
                        action,
                        &payload,
                        &env,
                        service.as_deref(),
                        user,
                        &auth,
                        timeout_secs,
                        priority,
                        &want,
                        expect.as_ref(),
                        marker
                            .take()
                            .unwrap_or_else(|| crate::history::JobCacheMarker {
                                key,
                                hit: None,
                                fields: Vec::new(),
                            }),
                        out,
                    )
                    .await;
                    return;
                }
                None => self.inner.actions.record_miss(service.as_deref()),
            }
            // The executor publishes what it produces, and it needs the
            // origin's key to publish it under — see `CACHE_KEY`.
            env.push((mesh_proto::CACHE_KEY.to_string(), key.to_display_string()));
        }
        let env = env;

        let spec = JobSpec {
            job_id,
            payload,
            env,
            timeout_secs,
            priority,
            submitter: me,
            submitter_user: user,
            auth: auth.clone(),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };

        // Try candidates best-first; a refusal that proves the job is not
        // there spills to the next one (MASTERPLAN §10.2, ENG-6): `Busy`, and
        // a candidate that could not be reached at all. A submission that
        // went out and got no answer is settled with the same executor
        // before anything moves — see `deliver` — because re-routing a job
        // that may have been accepted is how it runs twice.
        let mut accepted: Option<MemberInfo> = None;
        let mut last_busy = None;
        // Which refusals the candidates gave — full, or out of reach — so
        // the message below names the right problem.
        let (mut any_busy, mut any_unreachable) = (false, false);
        for target in ranked {
            let executor = target.node_id;
            let result = self.deliver(executor, spec.clone(), Duration::ZERO).await;
            match result {
                Ok(()) => {
                    // Recorded on the accept, not in the `Err` arms: a
                    // submission nobody took has no dispatch latency, it
                    // has a refusal, and mixing the two would put the
                    // scheduler's worst number in the same series as its
                    // ordinary one.
                    self.inner.dispatch.record(dispatch_began.elapsed());
                    accepted = Some(target);
                    break;
                }
                Err(e) if may_spill(&e) => {
                    info!(job = %job_id.short(), node = %target.name, "{e}; spilling over");
                    self.inner
                        .dispatch_failures
                        .spilled
                        .fetch_add(1, Ordering::Relaxed);
                    any_busy |= matches!(e, CoreError::Busy { .. });
                    any_unreachable |= matches!(e, CoreError::Unreachable(_));
                    last_busy = Some(e);
                }
                Err(e) => {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            }
        }
        let Some(target) = accepted else {
            let detail = last_busy
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no candidate accepted the job".into());
            // A device request that no *card* matches gets here rather than
            // to the "nobody advertises it" refusal above, because the
            // coarse `cap.gpus` filter cannot express a model (§9.1: the
            // tag is the filter, not the model). Reporting that as
            // saturation would be true about the executor's answer and
            // useless about the cause, so the request is named — the same
            // distinction the empty-candidate case already makes between a
            // sizing problem and a queueing one.
            // N2 P3: a demanded host port is the same shape of problem as
            // an unmatched device, and gets the same treatment. A node
            // already holding :8080 answers `Busy` — the refusal that makes
            // the job spill rather than double-bind — so reporting the
            // result as saturation would be true about the answer and
            // useless about the cause. A node with fifteen free slots is
            // not saturated, and telling an operator it is sends them to
            // add capacity for a problem capacity cannot fix.
            let wanted = mesh_proto::PortRequests::from_env(&spec.env);
            let volumes = mesh_proto::VolumeRequests::from_env(&spec.env);
            let static_ports: Vec<String> = wanted
                .0
                .iter()
                .filter_map(|p| p.fixed.map(|f| format!("{}:{f}", p.label)))
                .collect();
            let message = match (&device_req, static_ports.is_empty()) {
                (Some(req), _) => {
                    format!("no node could give this job {} ({detail})", req.describe())
                }
                // Deliberately *without* `{detail}`. The executor's literal
                // answer is the `Busy` refusal — "node is saturated (1
                // running, 0 queued)" — and quoting it after this sentence
                // contradicts it: a node with fifteen free slots is not
                // saturated, and the parenthetical would send the reader to
                // add capacity for a problem capacity cannot fix.
                (None, false) => format!(
                    "no node could give this job host port {} — every candidate already \
                     holds it, and a node refuses rather than double-binding. Use a dynamic \
                     port and read MESHWARE_PORT_{}, or free the one holding it.",
                    static_ports.join(", "),
                    wanted
                        .0
                        .iter()
                        .find(|p| p.is_static())
                        .map(|p| p.key_suffix())
                        .unwrap_or_default()
                ),
                (None, true) if !wanted.is_empty() => format!(
                    "no node could give this job {} host port(s) ({detail})",
                    wanted.len()
                ),
                // N5, and `{detail}` is left off for exactly the reason the
                // static-port arm leaves it off. Dispatch has already
                // filtered to nodes that *lend* these names, so getting
                // here means the name exists and the lease does not: a
                // writer is holding it. "Saturated" would send the reader
                // to add capacity for a problem capacity cannot fix.
                (None, true) if !volumes.is_empty() => {
                    let held = volumes.names().join(", ");
                    format!(
                        "no node could give this job volume {held} in the mode it asked \
                         for — a writer holds it, and one writer excludes everything. \
                         `meshnode volumes` on the candidate says who; claim it `:ro` if \
                         reading is enough."
                    )
                }
                (None, true) => match (any_busy, any_unreachable) {
                    (true, true) => {
                        format!("every matching node is saturated or unreachable ({detail})")
                    }
                    (false, true) => format!("no matching node could be reached ({detail})"),
                    _ => format!("every matching node is saturated ({detail})"),
                },
            };
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        };
        let executor = target.node_id;
        // C8 P1: the record is complete only now. Which node took the work
        // and whether the job had to spill past its best candidate are both
        // facts about the dispatch, not about the scoring, and a breakdown
        // that named a winner before the dispatch would contradict the job
        // it explains.
        explain.chosen = Some(executor);
        explain.spilled = explain
            .candidates
            .first()
            .is_some_and(|best| best.node != executor);
        self.stage_explain(job_id, explain);
        if let Some(marker) = &marker
            && let Some(history) = &self.inner.history
            && let Err(e) = history.put_cache_marker(&job_id, marker)
        {
            debug!(job = %job_id.short(), "cache marker write failed: {e}");
        }
        audit::record(
            &self.inner.engine,
            user,
            "job.run",
            &job_id.to_display_string(),
            "ok",
            &format!("{} on {}", spec.preview(), target.name),
            &auth.signature,
        );

        let _ = out
            .send(IpcResponse::JobStarted {
                job_id,
                executor,
                executor_name: target.name.clone(),
            })
            .await;

        // Eagerly pull the executor's fresh summary so this node can answer
        // jobs/cancel immediately.
        if executor != me {
            let gossip = self.inner.gossip.clone();
            let target = target.clone();
            tokio::spawn(async move {
                let _ = gossip.sync_with(&target).await;
            });
        }

        // C7 P4: a spot-check watches the stream go past and compares what
        // came out with what the cache claimed. It never alters the frames
        // the client sees — a verification run is an ordinary run to
        // everybody except the auditor.
        //
        // Not when the caller asked for captured rows: this path streams
        // without the M19 tee, so verifying a `--json` run would return the
        // right logs and no rows at all. Losing an occasional spot check is
        // strictly better than losing somebody's answer — and with
        // `verify_rate` at its default of zero, this never fires.
        if let Some(entry) = verifying
            && want.is_empty()
        {
            self.stream_and_verify(job_id, executor, entry, user, service.as_deref(), out)
                .await;
            return;
        }
        if want.is_empty() {
            // The pre-M19 path, byte for byte: no tee, no extra frames,
            // nothing between the executor's log and the client.
            self.stream_job_logs(job_id, executor, true, out).await;
            return;
        }
        self.stream_one_with_capture(job_id, executor, target.name.clone(), want, expect, out)
            .await;
    }

    /// Stream a spot-checked job and compare its result with the cache
    /// entry it was checking (C7 P4).
    ///
    /// **The comparison is of the observable answer** — exit code and
    /// output lengths — and deliberately not of timing, memory or anything
    /// else that legitimately differs between two correct runs. A
    /// disagreement raises an audit event and a loud frame; it does not
    /// fail the job, because the job is fine. What is not fine is the
    /// entry, and the honest response to that is to say so and drop it.
    pub(super) async fn stream_and_verify(
        &self,
        job_id: JobId,
        executor: NodeId,
        entry: mesh_proto::CacheEntry,
        user: UserId,
        service: Option<&str>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let (tx, mut rx) = mpsc::channel::<IpcResponse>(64);
        let this = self.clone();
        let streamer =
            tokio::spawn(async move { this.stream_job_logs(job_id, executor, true, tx).await });
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut exit_code = None;
        while let Some(frame) = rx.recv().await {
            if let IpcResponse::LogChunk { stream, data } = &frame {
                match stream {
                    1 => stdout.extend_from_slice(data),
                    _ => stderr.extend_from_slice(data),
                }
            }
            if let IpcResponse::JobDone {
                exit_code: code, ..
            } = &frame
            {
                exit_code = *code;
            }
            if out.send(frame).await.is_err() {
                break;
            }
        }
        let _ = streamer.await;

        let agreed = entry.agrees_with(exit_code, &stdout, &stderr);
        self.inner.actions.record_verification(agreed);
        if agreed {
            debug!(key = %entry.key.short(), "cache entry verified");
            return;
        }
        let detail = format!(
            "cached exit {:?} / {}+{} bytes; ran exit {:?} / {}+{} bytes",
            entry.exit_code,
            entry.stdout_len,
            entry.stderr_len,
            exit_code,
            stdout.len(),
            stderr.len()
        );
        warn!(key = %entry.key.short(), "cache entry disagreed with a fresh run: {detail}");
        // Dropped before the audit line is written, so the window in which
        // a known-bad entry can be served again is as short as this
        // function can make it.
        let (_, released) = self
            .inner
            .actions
            .purge(&mesh_proto::PurgeScope::Key(entry.key));
        self.inner.objects.unpin(&released);
        audit::record(
            &self.inner.engine,
            user,
            "cache.mismatch",
            &entry.key.to_display_string(),
            "mismatch",
            &format!(
                "{detail}; produced by {} on {}; entry dropped",
                entry.job.short(),
                entry.producer.short()
            ),
            &[],
        );
        let _ = service;
        let _ = out
            .send(IpcResponse::CacheMismatch {
                key: entry.key,
                detail,
            })
            .await;
    }

    /// Stream one job to the client and tee it into a `Result` row on the
    /// way past (M19 §5.4).
    ///
    /// A single target gets a row for the same reason a fan-out does:
    /// `--format json` must emit one array either way, so a script that
    /// grew from one node to six hundred does not change shape.
    pub(super) async fn stream_one_with_capture(
        &self,
        job_id: mesh_proto::JobId,
        executor: NodeId,
        executor_name: String,
        want: outputs::Wanted,
        expect: Option<crate::results::Expectation>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let mut capture = crate::results::RowCapture::new(&want);
        let (tx, mut rx) = mpsc::channel::<IpcResponse>(64);
        let this = self.clone();
        let streamer =
            tokio::spawn(async move { this.stream_job_logs(job_id, executor, true, tx).await });
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::LogChunk { stream, data } => {
                    if let Some(cap) = capture.as_mut() {
                        cap.chunk(stream, &data);
                    }
                    if out
                        .send(IpcResponse::LogChunk { stream, data })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                IpcResponse::JobDone { state, exit_code } => {
                    if let Some(cap) = capture.take() {
                        let mut row = cap.finish(
                            job_id,
                            executor,
                            executor_name.clone(),
                            state,
                            exit_code,
                            Some(mesh_proto::now_micros()),
                            |key, value| self.spill_output(job_id, key, value),
                        );
                        if let Some(expect) = &expect {
                            expect.judge(&mut row);
                        }
                        let failed_row = row.expect_passed == Some(false);
                        let detail = row.expect_detail.clone();
                        let name = row.node_name.clone();
                        let _ = out.send(IpcResponse::Result(Box::new(row))).await;
                        if let Some(expect) = &expect {
                            let _ = out
                                .send(IpcResponse::Expected {
                                    expression: expect.source.clone(),
                                    passed: u32::from(!failed_row),
                                    failed: if failed_row {
                                        vec![format!("{name}: {detail}")]
                                    } else {
                                        Vec::new()
                                    },
                                })
                                .await;
                        }
                    }
                    let _ = out.send(IpcResponse::JobDone { state, exit_code }).await;
                    break;
                }
                other => {
                    if out.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = streamer.await;
    }

    /// Build the sub-job spec for one planned fan-out target. Job ids come
    /// from the plan, so every dispatcher (origin or adopter) produces the
    /// identical spec — the idempotency key of §10.3.
    pub(super) fn fan_spec(plan: &FanoutPlanRecord, index: usize) -> JobSpec {
        let total = u32::try_from(plan.targets.len()).unwrap_or(u32::MAX);
        let target = &plan.targets[index];
        JobSpec {
            job_id: target.job_id,
            payload: plan.payload.clone(),
            env: plan.env.clone(),
            timeout_secs: plan.timeout_secs,
            priority: plan.priority,
            submitter: plan.submitter,
            submitter_user: plan.submitter_user,
            auth: plan.auth.clone(),
            fanout: Some(FanoutInfo {
                group: plan.group,
                index: u32::try_from(index).unwrap_or(u32::MAX),
                total,
            }),
            created_at_micros: plan.created_at_micros,
        }
    }

    /// Fan a payload out to many targets as one logical job (MASTERPLAN
    /// §9.3): per-target sub-jobs share a [`FanId`], streams multiplex with
    /// index attribution, and the terminal frame aggregates outcomes.
    ///
    /// M5 depth: the plan replicates *before* dispatch so a successor can
    /// finish it if this origin dies (§10.3); `max_parallel` dispatches in
    /// a sliding window; `halt_on_fail` stops dispatching once failures
    /// cross the threshold (remaining targets count as skipped). The origin
    /// owns the job: dispatch continues even if the submitting client hangs
    /// up mid-stream.
    #[allow(clippy::too_many_arguments)]
    /// **The join across every node this run touches** (ENG-21): the group
    /// id is on the replicated plan, so the owner, each executor and any
    /// successor that adopts the run after this node dies all log the same
    /// `group=`. `skip_all` because the arguments include the job payload
    /// and a signed envelope, and a span field is not the place for
    /// either.
    #[tracing::instrument(
        level = "info",
        name = "fanout",
        skip_all,
        fields(group = tracing::field::Empty)
    )]
    pub(super) async fn dispatch_fanout(
        &self,
        user: UserId,
        auth: SignedIpc,
        payload: JobPayload,
        env: Vec<(String, String)>,
        timeout_secs: Option<u64>,
        priority: JobPriority,
        controls: FanoutControls,
        targets: Vec<MemberInfo>,
        want: outputs::Wanted,
        expect: Option<crate::results::Expectation>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let me = self.node_id();
        let group = FanId::generate();
        let total = u32::try_from(targets.len()).unwrap_or(u32::MAX);

        let plan = FanoutPlanRecord {
            group,
            payload,
            env,
            timeout_secs,
            priority,
            submitter: me,
            submitter_user: user,
            auth: auth.clone(),
            targets: targets
                .iter()
                .map(|t| PlannedTarget {
                    job_id: mesh_proto::JobId::generate(),
                    node_id: t.node_id,
                    node_name: t.name.clone(),
                })
                .collect(),
            controls,
            created_at_micros: mesh_proto::now_micros(),
        };

        // Replicate the plan before the first dispatch: from this moment a
        // successor can complete the fan-out if we die (§10.3).
        if let Err(e) = self.inner.engine.author_op(OpBody::FanoutPlan {
            plan: Box::new(plan.clone()),
        }) {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("cannot record fan-out plan: {e}"),
                })
                .await;
            return;
        }
        self.inner.active_drives.lock().insert(group);
        tracing::Span::current().record("group", tracing::field::display(group.short()));
        // Counted from the moment the plan is replicated, which is the
        // moment this run exists (ENG-21): before it, a failure is a
        // refused submission and `meshware_dispatch_failures_total` has
        // it; after it, a successor can adopt the run even if this node
        // dies, so something began whatever happens here next.
        self.inner.groups.started(crate::observe::GroupKind::Fanout);
        let began = Instant::now();

        audit::record(
            &self.inner.engine,
            user,
            "job.run",
            &group.to_display_string(),
            "ok",
            &format!("fanout {} × {}", total, Self::fan_spec(&plan, 0).preview()),
            &auth.signature,
        );

        let started = IpcResponse::FanoutStarted {
            group,
            targets: plan
                .targets
                .iter()
                .map(|t| FanTarget {
                    job_id: t.job_id,
                    node_id: t.node_id,
                    node_name: t.node_name.clone(),
                })
                .collect(),
        };
        // The client hanging up stops *forwarding*, never dispatching.
        let mut client_gone = out.send(started).await.is_err();

        let window = controls
            .max_parallel
            .map(|n| (n.max(1)) as usize)
            .unwrap_or(plan.targets.len());
        let halt_at = controls.halt_on_fail.map(|p| p.threshold(total));
        // `--race N` (M9 §5.6, generalized from straggler speculation). The
        // policy rides the plan's environment rather than `controls`, so the
        // successor that adopts this group if we die races too — see
        // `mesh_proto::REQ_RACE`.
        let racing = mesh_proto::race_of(&plan.env).is_some();

        let (mtx, mut mrx) = mpsc::channel::<IpcResponse>(256);
        let mut mtx = Some(mtx);
        let mut next_index = 0usize;
        let mut in_flight = 0usize;
        let (mut succeeded, mut failed) = (0u32, 0u32);
        let mut halted = false;
        // Rows accumulate here on their way past. Nothing is replicated:
        // this is the owner's index and nobody else's (§5.3).
        let mut rows: Vec<mesh_proto::ResultRow> = Vec::new();

        // Initial wave, then keep the window full as targets finish. The
        // sender handle is dropped as soon as no launch can follow, so the
        // channel closes when the last sub-task ends.
        loop {
            while !halted
                && in_flight < window
                && next_index < plan.targets.len()
                && let Some(mtx) = &mtx
            {
                self.spawn_fan_subjob(&plan, next_index, &want, mtx.clone());
                next_index += 1;
                in_flight += 1;
            }
            if halted || next_index >= plan.targets.len() {
                mtx = None;
            }
            if in_flight == 0 {
                break;
            }
            let Some(frame) = mrx.recv().await else { break };
            if let IpcResponse::FanJobDone { state, .. } = &frame {
                in_flight -= 1;
                if *state == mesh_proto::JobState::Succeeded {
                    succeeded += 1;
                } else {
                    failed += 1;
                }
                if let Some(h) = halt_at
                    && failed >= h
                    && !halted
                {
                    halted = true;
                    info!(group = %group.short(), failed,
                        "halt-on-fail tripped; skipping remaining targets");
                }
                // A race ends the moment anybody wins. Nothing further is
                // dispatched, and every copy still running is cancelled —
                // the answer is in hand and the rest are now spending
                // somebody else's cores on an answer nobody will read.
                //
                // Cancellation is fire-and-forget on purpose: awaiting it
                // here would stall the frame loop that the cancelled jobs
                // report *into*, and a copy whose executor has gone
                // unreachable cannot be told to stop at all. Waiting for
                // either would give back exactly the latency this feature
                // exists to win.
                if racing && succeeded > 0 && !halted {
                    halted = true;
                    let losers: Vec<mesh_proto::JobId> = plan
                        .targets
                        .iter()
                        .take(next_index)
                        .map(|t| t.job_id)
                        .collect();
                    info!(group = %group.short(), copies = losers.len(),
                        "race won; cancelling the rest");
                    let this = self.clone();
                    tokio::spawn(async move {
                        for job_id in losers {
                            // Already-finished copies answer "not running",
                            // which is the expected case for the winner.
                            let _ = this.ipc_cancel(job_id).await;
                        }
                    });
                }
            }
            // Judge as the row goes past, so the client sees the verdict on
            // the row rather than only in the summary at the end.
            let frame = match frame {
                IpcResponse::Result(mut row) => {
                    if let Some(expect) = &expect {
                        expect.judge(&mut row);
                    }
                    rows.push((*row).clone());
                    IpcResponse::Result(row)
                }
                other => other,
            };
            if !client_gone && out.send(frame).await.is_err() {
                client_gone = true;
            }
        }
        let skipped = u32::try_from(plan.targets.len() - next_index).unwrap_or(u32::MAX);
        // `halted` outranks `failed`: a run stopped by `--halt-on-fail` or
        // by a won race has failures *because* it was stopped, and
        // reporting it as a plain failure would make deliberate early
        // stopping look like a fleet-wide outage on the graph.
        self.inner.groups.finished(
            crate::observe::GroupKind::Fanout,
            if halted {
                crate::observe::GroupOutcome::Halted
            } else if failed > 0 {
                crate::observe::GroupOutcome::Failed
            } else {
                crate::observe::GroupOutcome::Completed
            },
            began.elapsed(),
            u64::from(succeeded),
            u64::from(failed),
            u64::from(skipped),
        );

        if let Err(e) = self.inner.engine.author_op(OpBody::FanoutClosed {
            group,
            succeeded,
            failed,
            skipped,
        }) {
            warn!(group = %group.short(), "cannot close fan-out: {e}");
        }
        self.inner.active_drives.lock().remove(&group);

        // -- M19 results (§5.3, §5.5) ------------------------------------
        if !want.is_empty() {
            let set = mesh_proto::ResultSet {
                group,
                owner: me,
                owner_name: self.member_name(&me),
                submitter_user: user,
                preview: Self::fan_spec(&plan, 0).preview(),
                created_at_micros: plan.created_at_micros,
                missing: skipped,
                rows: rows.clone(),
            };
            self.record_results(&set);
            if let Some(expect) = &expect
                && !client_gone
            {
                let bad: Vec<String> = rows
                    .iter()
                    .filter(|r| r.expect_passed == Some(false))
                    .map(|r| {
                        if r.expect_detail.is_empty() {
                            r.node_name.clone()
                        } else {
                            format!("{}: {}", r.node_name, r.expect_detail)
                        }
                    })
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
        }

        if !client_gone {
            let _ = out
                .send(IpcResponse::FanoutDone {
                    succeeded,
                    failed,
                    skipped,
                })
                .await;
        }
    }

    /// Dispatch one planned target and pump its log stream into `mtx` with
    /// index attribution. Always ends with exactly one `FanJobDone`.
    ///
    /// When `want` is non-empty the streams are **teed** on the way past
    /// into a `Result` row, emitted just before that target's `FanJobDone`
    /// (M19 §5.4). A tee rather than a second read: the bytes are already
    /// crossing this node to reach the client, so `--json` costs what the
    /// plain fan-out costs.
    pub(super) fn spawn_fan_subjob(
        &self,
        plan: &FanoutPlanRecord,
        index: usize,
        want: &outputs::Wanted,
        mtx: mpsc::Sender<IpcResponse>,
    ) {
        let spec = Self::fan_spec(plan, index);
        let target_id = plan.targets[index].node_id;
        let target_name = plan.targets[index].node_name.clone();
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let mut capture = crate::results::RowCapture::new(want);
        let this = self.clone();
        tokio::spawn(async move {
            let job_id = spec.job_id;
            // A target is a target: there is nowhere to spill to, so an
            // unreachable one is given the time a restart takes (ENG-6).
            let dispatch = this.deliver(target_id, spec, FANOUT_PATIENCE).await;
            if let Err(e) = dispatch {
                let detail = format!("meshware: dispatch to {target_name}: {e}\n");
                let _ = mtx
                    .send(IpcResponse::FanChunk {
                        index,
                        stream: 2,
                        data: detail.clone().into_bytes(),
                    })
                    .await;
                // A target that never started still gets a row. A table
                // with 599 lines and no explanation for the 600th is the
                // failure mode this whole milestone exists to remove.
                if let Some(mut cap) = capture.take() {
                    cap.chunk(2, detail.as_bytes());
                    let row = cap.finish(
                        job_id,
                        target_id,
                        target_name.clone(),
                        mesh_proto::JobState::Failed,
                        None,
                        Some(mesh_proto::now_micros()),
                        |key, value| this.spill_output(job_id, key, value),
                    );
                    let _ = mtx.send(IpcResponse::Result(Box::new(row))).await;
                }
                let _ = mtx
                    .send(IpcResponse::FanJobDone {
                        index,
                        state: mesh_proto::JobState::Failed,
                        exit_code: None,
                    })
                    .await;
                return;
            }
            if target_id != this.node_id() {
                // Eagerly pull the executor's fresh summary (as the
                // single-target path does).
                let member = this
                    .inner
                    .engine
                    .read(|s| s.members.get(&target_id).map(|m| m.info.clone()));
                if let Some(member) = member {
                    let gossip = this.inner.gossip.clone();
                    tokio::spawn(async move {
                        let _ = gossip.sync_with(&member).await;
                    });
                }
            }
            let (ttx, mut trx) = mpsc::channel::<IpcResponse>(64);
            let streamer = this.clone();
            tokio::spawn(async move {
                streamer.stream_job_logs(job_id, target_id, true, ttx).await;
            });
            while let Some(frame) = trx.recv().await {
                let mapped = match frame {
                    IpcResponse::LogChunk { stream, data } => {
                        if let Some(cap) = capture.as_mut() {
                            cap.chunk(stream, &data);
                        }
                        IpcResponse::FanChunk {
                            index,
                            stream,
                            data,
                        }
                    }
                    IpcResponse::JobDone { state, exit_code } => {
                        if let Some(cap) = capture.take() {
                            let row = cap.finish(
                                job_id,
                                target_id,
                                target_name.clone(),
                                state,
                                exit_code,
                                Some(mesh_proto::now_micros()),
                                |key, value| this.spill_output(job_id, key, value),
                            );
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
                            let row = cap.finish(
                                job_id,
                                target_id,
                                target_name.clone(),
                                mesh_proto::JobState::Failed,
                                None,
                                Some(mesh_proto::now_micros()),
                                |key, value| this.spill_output(job_id, key, value),
                            );
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
                    other => other,
                };
                if mtx.send(mapped).await.is_err() {
                    return;
                }
            }
            // Stream ended without a terminal frame: report as failed.
            if let Some(cap) = capture.take() {
                let row = cap.finish(
                    job_id,
                    target_id,
                    target_name.clone(),
                    mesh_proto::JobState::Failed,
                    None,
                    Some(mesh_proto::now_micros()),
                    |key, value| this.spill_output(job_id, key, value),
                );
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
    }
}
