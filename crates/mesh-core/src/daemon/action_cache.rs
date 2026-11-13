//! The action cache: what the mesh has already computed, and the proofs
//! that let a node index somebody else's entry.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// A `map` item answered from the cache, as the outcome the runner
    /// would have produced.
    ///
    /// An item's result is its stdout, stderr and exit code, all of which
    /// are small and all of which the entry holds — so unlike a job hit
    /// this one needs no files written anywhere. `None` on a miss *and* on
    /// a materialisation failure: running it is always correct.
    pub(super) async fn cached_item(
        &self,
        action: &mesh_proto::ActionSpec,
        item: &str,
        as_object: bool,
    ) -> Option<mesh_proto::ItemOutcome> {
        let key = action.key();
        let entry = match self.cache_lookup(&key).await {
            Some(entry) => entry,
            None => {
                self.inner.actions.record_miss(Some("(map)"));
                return None;
            }
        };
        // An object-captured hit must come back as an *object reference*,
        // not as text: that is what `map` promises and what `--out` fetches.
        // The bytes are now in this node's store, so this node is a
        // legitimate holder and says so.
        let output = entry.outputs.first().cloned();
        if as_object && output.is_none() {
            // An inline entry cannot answer an object-captured item. Not an
            // error — the two capture modes are different actions in every
            // way that matters — so it is a miss and the item runs.
            self.inner.actions.record_miss(Some("(map)"));
            return None;
        }
        let fetched = match self.cache_materialise(&entry, entry.job).await {
            Ok(fetched) => fetched,
            Err(e) => {
                debug!(key = %key.short(), "map item hit could not be materialised: {e}");
                self.inner.actions.record_miss(Some("(map)"));
                return None;
            }
        };
        let text = |id: Option<mesh_proto::ObjectId>| -> String {
            id.and_then(|id| fetched.iter().find(|(fid, _)| *fid == id))
                .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        };
        self.inner.actions.record_hit(Some("(map)"), entry.bytes());
        let object = output.map(|o| mesh_proto::ObjectRef {
            id: o.object,
            size: o.size,
            holder: self.node_id(),
        });
        if let Some(obj) = &object {
            // The copy this node just fetched is a copy the directory
            // should know about — otherwise the next fetcher is sent to the
            // original producer for bytes that are now on two nodes.
            self.announce_objects(entry.job, &[(obj.id, obj.size)])
                .await;
        }
        Some(mesh_proto::ItemOutcome {
            item: item.to_string(),
            exit_code: entry.exit_code,
            stdout: if as_object {
                String::new()
            } else {
                text(entry.stdout)
            },
            stderr: text(entry.stderr),
            // Zero, and truthfully so: this item took no time because it
            // did not run. A cached item that reported the *original* run's
            // duration would make a `map` summary say the mesh spent an
            // hour it did not spend.
            micros: 0,
            output: object,
        })
    }

    /// Publish one finished `map` item.
    pub(super) async fn publish_item(
        &self,
        plan: &mesh_proto::ParallelPlan,
        action: &mesh_proto::ActionSpec,
        outcome: &mesh_proto::ItemOutcome,
    ) {
        let job = plan.job;
        let mut announce = Vec::new();
        let store = |text: &str| -> Option<(mesh_proto::ObjectId, u64)> {
            if text.is_empty() {
                return None;
            }
            match self.inner.objects.put(job, text.as_bytes()) {
                Ok(obj) => Some((obj.id, obj.size)),
                Err(e) => {
                    debug!("cache item store failed: {e}");
                    None
                }
            }
        };
        let stdout = store(&outcome.stdout);
        let stderr = store(&outcome.stderr);
        announce.extend(stdout);
        announce.extend(stderr);
        // Under `map` the item's answer is already an object — stored and
        // announced by `capture_objects` — so the entry references it
        // rather than storing a second copy of the same bytes under a
        // second id.
        let outputs: Vec<mesh_proto::CachedOutput> = outcome
            .output
            .iter()
            .map(|obj| mesh_proto::CachedOutput {
                // No path: a map item's output is not a file in a working
                // directory, it is the item's whole answer.
                path: String::new(),
                object: obj.id,
                size: obj.size,
            })
            .collect();
        let pinned: Vec<mesh_proto::ObjectId> = announce
            .iter()
            .map(|(id, _)| *id)
            .chain(outputs.iter().map(|o| o.object))
            .collect();
        self.inner
            .objects
            .pin(&pinned, self.inner.config.cache.pin_lease);
        self.announce_objects(job, &announce).await;
        let entry = mesh_proto::CacheEntry {
            key: action.key(),
            exit_code: outcome.exit_code,
            stdout: stdout.map(|(id, _)| id),
            stdout_len: outcome.stdout.len() as u64,
            stderr: stderr.map(|(id, _)| id),
            stderr_len: outcome.stderr.len() as u64,
            outputs,
            producer: self.node_id(),
            job,
            epoch: self.inner.actions.epoch(),
            hlc: self.inner.engine.clock().now(),
            at_micros: mesh_proto::now_micros(),
        };
        let proof = mesh_proto::CacheProof::Item {
            item: outcome.item.clone(),
            plan: None,
        };
        self.cache_publish(entry, Some("(map)".into()), proof, Some(plan))
            .await;
    }

    /// Publish an entry for every cacheable job this node has just finished
    /// (C7 P2).
    ///
    /// **On the executor, because the outputs are on the executor.** That is
    /// G4 restated: a worker's output stays on the worker, the directory
    /// holds a hundred bytes saying where, and whoever needs the bytes
    /// fetches them straight from the producer. Publishing from the origin
    /// would funnel every cached byte in the mesh through whoever submitted
    /// the job — exactly the central bottleneck the object store exists to
    /// avoid.
    ///
    /// Off the job's own path, on the history task's timer: publishing is
    /// the cheap half of the milestone and it must never be between a job
    /// finishing and a user seeing that it did.
    pub(super) async fn publish_finished_actions(&self) {
        if !self.inner.actions.enabled() {
            return;
        }
        let me = self.node_id();
        // Local, terminal, successful, and not already published. Failures
        // are not cached: an action that failed is very often an action
        // whose environment was wrong in a way the key does not capture —
        // a missing file, a full disk, a service that was down — and
        // serving that back for a week is the cache's worst possible
        // behaviour. A *deliberate* non-zero exit that is genuinely
        // deterministic is indistinguishable from that one, and the safe
        // side of an indistinguishable pair is the one that re-runs.
        let candidates: Vec<(JobId, u64)> = self.inner.engine.read(|s| {
            s.jobs
                .values()
                .filter(|j| j.executor == me && j.state == JobState::Succeeded)
                .map(|j| (j.job_id, j.finished_at_micros.unwrap_or(0)))
                .collect()
        });
        for (job, finished_at) in candidates {
            // Marked *before* the spec is read, and for every candidate
            // rather than only the cacheable ones. `spec_of` is a disk
            // read; a set that only remembered the jobs it published would
            // re-read every finished job on this node from disk on every
            // tick, forever, and that cost grows with the node's history
            // rather than with its work.
            if !self.mark_considered(job, finished_at) {
                continue;
            }
            let Some(spec) = self.inner.jobs.spec_of(&job) else {
                continue;
            };
            let Some(key) = mesh_proto::cache::key_of(&spec.env) else {
                continue;
            };
            // A spot-check must not overwrite the entry it is checking: if
            // the entry is wrong, republishing the fresh result would hide
            // the disagreement the run existed to find.
            if mesh_proto::cache::is_verification(&spec.env) {
                continue;
            }
            if let Err(e) = self.publish_one_action(&spec, key).await {
                debug!(job = %job.short(), "cache publish skipped: {e}");
            }
        }
    }

    /// Remember that this job has been looked at, returning `false` if it
    /// already had been.
    ///
    /// **A watermark, not a wipe** (ENG-19). This was a `HashSet<JobId>`
    /// cleared wholesale at fifty thousand entries, on the grounds that a
    /// set of ids has no age to prune by. It does: a finished job carries
    /// the instant it finished, which is exactly the ordering this needs.
    /// Wiping meant a node with more than fifty thousand finished jobs
    /// forgot *everything* and re-read every one of their specs from disk
    /// on the next tick — the largest burst of disk reads on the busiest
    /// node, triggered by nothing but its own history.
    ///
    /// So: everything that finished at or before `through` has been looked
    /// at, and `recent` holds the ids after it. When `recent` fills, the
    /// watermark advances past the older half and those ids are dropped —
    /// still remembered, now by the watermark instead of one at a time.
    pub(super) fn mark_considered(&self, job: JobId, finished_at: u64) -> bool {
        /// Ids held individually before the watermark absorbs the older
        /// half. Halving rather than trimming keeps the sort amortised at
        /// one per `CONSIDERED_MAX / 2` insertions.
        const CONSIDERED_MAX: usize = 50_000;
        let mut seen = self.inner.published_actions.lock();
        if finished_at != 0 && finished_at <= seen.through_micros {
            return false;
        }
        if seen.recent.insert(job, finished_at).is_some() {
            return false;
        }
        if seen.recent.len() > CONSIDERED_MAX {
            let mut times: Vec<u64> = seen.recent.values().copied().collect();
            times.sort_unstable();
            let median = times[times.len() / 2];
            seen.through_micros = seen.through_micros.max(median);
            let mark = seen.through_micros;
            // `> mark`, so the job *at* the watermark is not re-considered:
            // the `<=` test above already speaks for it.
            seen.recent.retain(|_, at| *at > mark);
        }
        true
    }

    /// Put one finished job's outputs in the object store and announce the
    /// entry.
    pub(super) async fn publish_one_action(
        &self,
        spec: &JobSpec,
        key: mesh_proto::ActionKey,
    ) -> Result<()> {
        let dir = self.inner.jobs.job_dir_of(&spec.job_id);
        let status = self
            .inner
            .jobs
            .status_of(&spec.job_id)
            .ok_or_else(|| CoreError::Job("no status for a finished job".into()))?;
        let job = spec.job_id;
        let mut announce: Vec<(mesh_proto::ObjectId, u64)> = Vec::new();
        // Off the async workers (ENG-26): a cached action's output can be
        // gigabytes, and hashing it here would hold a runtime thread for
        // the whole of it.
        let put = async |path: &std::path::Path| -> Option<(mesh_proto::ObjectId, u64)> {
            match std::fs::metadata(path) {
                Ok(meta) if meta.len() > 0 => {
                    match self.inner.objects.put_file_off_thread(job, path).await {
                        Ok(obj) => Some((obj.id, obj.size)),
                        Err(e) => {
                            debug!(path = %path.display(), "cache output store failed: {e}");
                            None
                        }
                    }
                }
                // An empty stream is a real result and needs no object:
                // zero bytes are cheaper to reproduce than to reference.
                _ => None,
            }
        };
        let stdout_path = dir.join("stdout.log");
        let stderr_path = dir.join("stderr.log");
        let stdout = put(&stdout_path).await;
        let stderr = put(&stderr_path).await;
        let stdout_len = std::fs::metadata(&stdout_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let stderr_len = std::fs::metadata(&stderr_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let mut outputs = Vec::new();
        let globs = mesh_proto::cache::output_globs(&spec.env);
        if !globs.is_empty() {
            // Where the process actually ran, which is **not** the same
            // directory for every payload class: a command or an inline
            // script runs in `work/`, and a service runs in its bundle's
            // checkout under its declared `working_dir`. Globbing `work/`
            // for a service would find nothing and cache an empty output
            // list — silently, which is the worst way for this to be wrong.
            let work = match &spec.payload {
                JobPayload::Service(exec) if exec.working_dir.is_empty() => dir.join("checkout"),
                JobPayload::Service(exec) => dir.join("checkout").join(&exec.working_dir),
                JobPayload::Command(_) | JobPayload::Script(_) => dir.join("work"),
            };
            for (rel, path) in collect_outputs(&work, &globs) {
                if let Some((id, size)) = put(&path).await {
                    outputs.push(mesh_proto::CachedOutput {
                        path: rel,
                        object: id,
                        size,
                    });
                }
            }
        }
        announce.extend(stdout);
        announce.extend(stderr);
        announce.extend(outputs.iter().map(|o| (o.object, o.size)));
        // C7 P3: pin before announcing. An object's refcount is the job
        // that made it, and that job is finished — so without this the
        // entry would outlive its bytes by the object TTL and every hit
        // after fifteen minutes would fail to materialise.
        let pinned: Vec<mesh_proto::ObjectId> = announce.iter().map(|(id, _)| *id).collect();
        self.inner
            .objects
            .pin(&pinned, self.inner.config.cache.pin_lease);
        // The objects are announced to the *object* directory first. An
        // entry whose objects nobody can locate is an entry that will fail
        // to materialise, which costs a fetch before it costs the execution
        // it was going to cost anyway.
        self.announce_objects(job, &announce).await;

        let entry = mesh_proto::CacheEntry {
            key,
            exit_code: status.exit_code,
            stdout: stdout.map(|(id, _)| id),
            stdout_len,
            stderr: stderr.map(|(id, _)| id),
            stderr_len,
            outputs,
            producer: self.node_id(),
            job,
            epoch: self.inner.actions.epoch(),
            hlc: self.inner.engine.clock().now(),
            at_micros: mesh_proto::now_micros(),
        };
        let service = match &spec.payload {
            JobPayload::Service(exec) => Some(exec.service_name.clone()),
            _ => None,
        };
        info!(
            job = %job.short(),
            key = %key.short(),
            bytes = entry.bytes(),
            "published a cache entry"
        );
        let proof = mesh_proto::CacheProof::Job(Box::new(spec.clone()));
        self.cache_publish(entry, service, proof, None).await;
        Ok(())
    }

    /// Answer a submission from the cache (C7 P2).
    ///
    /// **A hit is visible or it is untrustworthy** (§11.2). The client gets
    /// a [`IpcResponse::CacheHit`] frame *before* the output it stands in
    /// for, a real job record it can inspect afterwards, and a marker in
    /// the history index that survives the session. "It didn't run" and "it
    /// broke" are indistinguishable unless the product says which, and the
    /// difference between those two is the difference between a feature and
    /// a bug report factory.
    ///
    /// **Any failure falls through to executing.** A hit that cannot fetch
    /// its objects — the producer left, a chunk is gone, the bytes never
    /// made it — is not a hit; it is a slow miss, and the caller running
    /// the job is always correct.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn serve_cache_hit(
        &self,
        job_id: JobId,
        entry: mesh_proto::CacheEntry,
        action: &mesh_proto::ActionSpec,
        payload: &JobPayload,
        env: &[(String, String)],
        service: Option<&str>,
        user: UserId,
        auth: &SignedIpc,
        timeout_secs: Option<u64>,
        priority: JobPriority,
        // What the caller asked to capture (M19 §5.2), so a hit answers
        // the same question a miss does.
        want: &outputs::Wanted,
        expect: Option<&crate::results::Expectation>,
        mut marker: crate::history::JobCacheMarker,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let me = self.node_id();
        let fetched = match self.cache_materialise(&entry, job_id).await {
            Ok(fetched) => fetched,
            Err(e) => {
                // Not an error the user sees: it is a miss with a slow
                // start, and the correct response is the one the caller
                // would have taken anyway.
                warn!(
                    key = %entry.key.short(),
                    "cache hit could not be materialised ({e}); running the job"
                );
                self.inner.actions.record_miss(service);
                self.dispatch_after_failed_hit(
                    job_id,
                    payload,
                    env,
                    action,
                    user,
                    auth,
                    timeout_secs,
                    priority,
                    out,
                )
                .await;
                return;
            }
        };
        let bytes_of = |id: Option<mesh_proto::ObjectId>| -> Vec<u8> {
            id.and_then(|id| {
                fetched
                    .iter()
                    .find(|(fid, _)| *fid == id)
                    .map(|(_, b)| b.clone())
            })
            .unwrap_or_default()
        };
        let stdout = bytes_of(entry.stdout);
        let stderr = bytes_of(entry.stderr);
        let outputs: Vec<(String, Vec<u8>)> = entry
            .outputs
            .iter()
            .map(|o| (o.path.clone(), bytes_of(Some(o.object))))
            .collect();

        let spec = JobSpec {
            job_id,
            payload: payload.clone(),
            env: env.to_vec(),
            timeout_secs,
            priority,
            submitter: me,
            submitter_user: user,
            auth: auth.clone(),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };
        if let Err(e) =
            self.inner
                .jobs
                .record_cached(&spec, entry.exit_code, &stdout, &stderr, &outputs)
        {
            warn!(job = %job_id.short(), "recording a cache hit failed ({e}); running the job");
            self.inner.actions.record_miss(service);
            self.dispatch_after_failed_hit(
                job_id,
                payload,
                env,
                action,
                user,
                auth,
                timeout_secs,
                priority,
                out,
            )
            .await;
            return;
        }

        let saved = entry.bytes();
        self.inner.actions.record_hit(service, saved);
        let hit = mesh_proto::CacheHit {
            key: entry.key,
            produced_by: entry.job,
            producer: entry.producer,
            bytes_saved: saved,
            verified: false,
        };
        marker.hit = Some(hit.clone());
        if let Some(history) = &self.inner.history
            && let Err(e) = history.put_cache_marker(&job_id, &marker)
        {
            debug!(job = %job_id.short(), "cache marker write failed: {e}");
        }
        audit::record(
            &self.inner.engine,
            user,
            "job.cached",
            &job_id.to_display_string(),
            "ok",
            &format!(
                "{} served from {} (produced by {})",
                spec.preview(),
                entry.key.short(),
                entry.job.short()
            ),
            &auth.signature,
        );

        // Kept for the answer row below, before the loop consumes them.
        let (stdout_bytes, stderr_bytes) = (stdout.clone(), stderr.clone());
        // The frames a real run would have sent, in the same order: the
        // client's job-following code needs no branch for this, which is
        // what keeps a cached run inspectable by every tool that already
        // exists.
        let _ = out.send(IpcResponse::CacheHit(Box::new(hit))).await;
        let _ = out
            .send(IpcResponse::JobStarted {
                job_id,
                executor: me,
                executor_name: self.inner.config.node_name.clone(),
            })
            .await;
        for (stream, data) in [(1u8, stdout), (2u8, stderr)] {
            if data.is_empty() {
                continue;
            }
            for chunk in data.chunks(64 * 1024) {
                if out
                    .send(IpcResponse::LogChunk {
                        stream,
                        data: chunk.to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        let state = if entry.exit_code == Some(0) {
            JobState::Succeeded
        } else {
            JobState::Failed
        };
        // The answer row (M19 §5.4), rebuilt from the cached log.
        //
        // **A hit used to lose this**, and the shape of that bug is the
        // worst one a cache can have: a caller that asked for outputs got
        // them on a miss and nothing on a hit, so the feature worked
        // perfectly until the second time anybody used it. Rebuilding here
        // costs one parse of a log this node already has in memory, and the
        // values come out identical because the marker line is *in* the
        // stdout the entry cached — the same text, through the same parser,
        // as the run that produced it.
        if let Some(mut capture) = crate::results::RowCapture::new(want) {
            capture.chunk(1, &stdout_bytes);
            capture.chunk(2, &stderr_bytes);
            let mut row = capture.finish(
                job_id,
                me,
                self.inner.config.node_name.clone(),
                state,
                entry.exit_code,
                Some(mesh_proto::now_micros()),
                |key, value| self.spill_output(job_id, key, value),
            );
            if let Some(expect) = expect {
                expect.judge(&mut row);
            }
            let _ = out.send(IpcResponse::Result(Box::new(row))).await;
        }
        let _ = out
            .send(IpcResponse::JobDone {
                state,
                exit_code: entry.exit_code,
            })
            .await;
    }

    /// The fallback when a hit could not be honoured: run it after all.
    ///
    /// Deliberately the plain single-target dispatch and not a retry of the
    /// whole submission — placement and the grant check have both already
    /// happened for this job, and re-running them would double the
    /// audit trail for one unlucky fetch.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_after_failed_hit(
        &self,
        job_id: JobId,
        payload: &JobPayload,
        env: &[(String, String)],
        action: &mesh_proto::ActionSpec,
        user: UserId,
        auth: &SignedIpc,
        timeout_secs: Option<u64>,
        priority: JobPriority,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let mut env = env.to_vec();
        env.push((
            mesh_proto::CACHE_KEY.to_string(),
            action.key().to_display_string(),
        ));
        let spec = JobSpec {
            job_id,
            payload: payload.clone(),
            env,
            timeout_secs,
            priority,
            submitter: self.node_id(),
            submitter_user: user,
            auth: auth.clone(),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };
        match self.accept_job(spec).await {
            Ok(()) => {
                let executor = self.node_id();
                let _ = out
                    .send(IpcResponse::JobStarted {
                        job_id,
                        executor,
                        executor_name: self.inner.config.node_name.clone(),
                    })
                    .await;
                self.stream_job_logs(job_id, executor, true, out).await;
            }
            Err(e) => {
                let _ = out
                    .send(IpcResponse::Error {
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }

    /// Whether this node's cache shard holds anything (C7).
    ///
    /// A test seam, and a narrow one on purpose: the interesting assertions
    /// are made through `CacheStats` over IPC, exactly as an operator makes
    /// them. This exists only so a test can wait for the publish tick
    /// without polling a whole stats response.
    #[doc(hidden)]
    pub fn action_cache_is_empty(&self) -> bool {
        self.inner.actions.is_empty()
    }

    /// Live members, for the cache directory's rendezvous ranking.
    ///
    /// Every live member, not just the ones this node can speak 1.14 to —
    /// see [`crate::cache::directory_for`] for why an inconsistent ranking
    /// would be worse than a skipped replica.
    pub(super) fn cache_members(&self) -> Vec<NodeId> {
        self.inner.engine.read(|s| {
            s.members
                .keys()
                .copied()
                .filter(|id| {
                    s.is_active_member(id)
                        && !matches!(self.inner.gossip.liveness_of(id), Liveness::Dead)
                })
                .collect()
        })
    }

    /// Ask the directory whether this action has already been run.
    ///
    /// **A miss costs one request.** The directory nodes are arithmetic;
    /// this node is often one of them, in which case the answer is a hash
    /// lookup and no network at all. A replica that is unreachable, too
    /// old, or simply does not have it is skipped and the next is tried;
    /// when both come up empty the caller runs the job, which is what it
    /// was going to do anyway.
    pub(super) async fn cache_lookup(
        &self,
        key: &mesh_proto::ActionKey,
    ) -> Option<mesh_proto::CacheEntry> {
        if !self.inner.actions.enabled() {
            return None;
        }
        let me = self.node_id();
        for node in crate::cache::directory_for(key, self.cache_members()) {
            if node == me {
                if let Some(entry) = self.inner.actions.get(key) {
                    return Some(entry);
                }
                continue;
            }
            let addrs = self
                .inner
                .engine
                .read(|s| s.members.get(&node).map(|m| m.info.addrs.clone()))
                .unwrap_or_default();
            let Ok(conn) = self.inner.transport.member_conn(node, &addrs).await else {
                continue;
            };
            if self.inner.transport.peer_minor(&node).unwrap_or(0) < mesh_proto::CACHE_MINOR {
                continue;
            }
            match self
                .inner
                .transport
                .request_one(&conn, &Request::CacheLookup { key: *key })
                .await
            {
                Ok(Response::CacheEntry(Some(entry))) => return Some(*entry),
                _ => continue,
            }
        }
        None
    }

    /// Tell the directory what this node just produced.
    ///
    /// Fire and forget, and deliberately not retried: a lost publication
    /// costs a re-execution later, not correctness, exactly as a lost
    /// `ObjectAnnounce` does (M9 §10, Q3). Publishing is the cheap half of
    /// the milestone and it must never be on the critical path of a job
    /// reporting done.
    pub(super) async fn cache_publish(
        &self,
        entry: mesh_proto::CacheEntry,
        service: Option<String>,
        proof: mesh_proto::CacheProof,
        plan: Option<&mesh_proto::ParallelPlan>,
    ) {
        if !self.inner.actions.enabled() {
            return;
        }
        let me = self.node_id();
        self.inner.actions.record_published();
        let sends = crate::cache::directory_for(&entry.key, self.cache_members())
            .into_iter()
            .filter_map(|node| {
                if node == me {
                    let released = self
                        .inner
                        .actions
                        .put_and_evict(entry.clone(), service.clone());
                    self.inner.objects.unpin(&released);
                    return None;
                }
                let entry = entry.clone();
                let proof = proof.clone();
                Some(async move {
                    let addrs = self
                        .inner
                        .engine
                        .read(|s| s.members.get(&node).map(|m| m.info.addrs.clone()))
                        .unwrap_or_default();
                    let Ok(conn) = self.inner.transport.member_conn(node, &addrs).await else {
                        return;
                    };
                    // SEC-18: a replica below this refuses nothing, and one
                    // at it takes nothing else.
                    if self.inner.transport.peer_minor(&node).unwrap_or(0)
                        < mesh_proto::CACHE_PROOF_MINOR
                    {
                        return;
                    }
                    let request = Request::CachePublishV2 {
                        entry: Box::new(entry.clone()),
                        proof: Box::new(proof),
                    };
                    let answer = self.inner.transport.request_one(&conn, &request).await;
                    // A `map` item goes without its plan, and with it only
                    // to a replica that has not seen that plan yet.
                    let answer = match (answer, plan) {
                        (Ok(Response::CachePlanNeeded), Some(plan)) => {
                            let Request::CachePublishV2 { entry, proof } = request else {
                                return;
                            };
                            let proof = match *proof {
                                mesh_proto::CacheProof::Item { item, .. } => {
                                    mesh_proto::CacheProof::Item {
                                        item,
                                        plan: Some(Box::new(plan.clone())),
                                    }
                                }
                                other => other,
                            };
                            let request = Request::CachePublishV2 {
                                entry,
                                proof: Box::new(proof),
                            };
                            self.inner.transport.request_one(&conn, &request).await
                        }
                        (answer, _) => answer,
                    };
                    match answer {
                        Ok(Response::Ok) => {}
                        Ok(Response::Error { message }) => {
                            debug!(node = %node.short(), "cache entry refused: {message}");
                        }
                        Ok(_) => debug!(node = %node.short(), "cache publish: unexpected answer"),
                        Err(e) => debug!(node = %node.short(), "cache publish failed: {e}"),
                    }
                })
            });
        futures::future::join_all(sends).await;
    }

    /// Is `entry`, published by `peer`, what `proof` says it is (SEC-18)?
    /// On success, the service to file it under.
    ///
    /// Three things, in the order they are cheap to check: the publisher
    /// ran the work, the work was authorized by a person allowed to run it
    /// there, and the work derives this key. The third is the one that
    /// matters — the key is what another person's run looks up — and the
    /// first two are what keep a member from pointing at somebody else's
    /// job to borrow its authorization.
    pub(super) async fn verify_cache_publish(
        &self,
        peer: NodeId,
        entry: &mesh_proto::CacheEntry,
        proof: mesh_proto::CacheProof,
    ) -> std::result::Result<Option<String>, CacheRefusal> {
        use CacheRefusal::Refused;
        if entry.epoch != self.inner.actions.epoch() {
            return Err(Refused(
                "the entry was computed under another cache epoch".into(),
            ));
        }
        let derives = |derive: &dyn Fn(&str) -> Option<mesh_proto::ActionKey>| {
            crate::cache::known_arches()
                .iter()
                .any(|arch| derive(arch) == Some(entry.key))
        };
        match proof {
            mesh_proto::CacheProof::Job(spec) => {
                if spec.job_id != entry.job {
                    return Err(Refused("the entry cites a different job".into()));
                }
                if mesh_proto::cache::is_verification(&spec.env) {
                    return Err(Refused("a verification run is not published".into()));
                }
                // As this node's state records the job — which may be a
                // moment behind the node that just finished it.
                let ran = || {
                    self.inner.engine.read(|s| {
                        s.jobs.get(&spec.job_id).is_some_and(|j| {
                            j.executor == peer
                                && j.state == JobState::Succeeded
                                && j.submitter == spec.submitter
                                && j.submitter_user == spec.submitter_user
                        })
                    })
                };
                if !ran() {
                    self.pull_state_from(peer).await;
                    if !ran() {
                        return Err(Refused(format!(
                            "{} did not run job {} to success",
                            self.member_name(&peer),
                            spec.job_id.short()
                        )));
                    }
                }
                self.verify_job_auth_for(&spec, peer, false)
                    .map_err(|e| Refused(e.to_string()))?;
                if mesh_proto::cache::key_of(&spec.env) != Some(entry.key)
                    || !derives(&|arch| {
                        crate::cache::derive(&spec, entry.epoch, arch)
                            .ok()
                            .map(|a| a.key())
                    })
                {
                    return Err(Refused("the job does not derive this key".into()));
                }
                Ok(match &spec.payload {
                    JobPayload::Service(exec) => Some(exec.service_name.clone()),
                    _ => None,
                })
            }
            mesh_proto::CacheProof::Item { item, plan } => {
                let verified = match plan {
                    Some(plan) => {
                        if plan.job != entry.job {
                            return Err(Refused("the entry cites a different job".into()));
                        }
                        if !plan.workers.contains(&peer) {
                            return Err(Refused(format!(
                                "{} was not a worker of job {}",
                                self.member_name(&peer),
                                plan.job.short()
                            )));
                        }
                        self.verify_plan_auth_for(&plan, peer, peer)
                            .map_err(|e| Refused(e.to_string()))?;
                        self.inner.verified_plans.lock().insert(*plan)
                    }
                    None => self
                        .inner
                        .verified_plans
                        .lock()
                        .get(&entry.job)
                        .ok_or(CacheRefusal::PlanNeeded)?,
                };
                if !verified.plan.workers.contains(&peer) {
                    return Err(Refused("the publisher was not a worker of this job".into()));
                }
                if !verified.items.contains(item.as_str()) {
                    return Err(Refused("the item is not one of the job's".into()));
                }
                if !derives(&|arch| {
                    plan_item_action(&verified.plan, &item, entry.epoch, arch).map(|a| a.key())
                }) {
                    return Err(Refused("the item does not derive this key".into()));
                }
                Ok(Some("(map)".into()))
            }
        }
    }

    /// Pull every object an entry names into this node's store.
    ///
    /// **All or nothing.** A hit that can only fetch half its outputs is
    /// not a hit; it is a miss that has already cost a fetch, and
    /// half-materialising would hand the caller a job directory that looks
    /// finished and is not. On any failure the caller falls through to
    /// executing, which is always correct.
    pub(super) async fn cache_materialise(
        &self,
        entry: &mesh_proto::CacheEntry,
        job: JobId,
    ) -> Result<Vec<(mesh_proto::ObjectId, Vec<u8>)>> {
        let me = self.node_id();
        let mut out = Vec::new();
        for id in entry.objects() {
            let hints = [entry.producer];
            let manifest = if let Some(m) = self.inner.objects.manifest(&id)
                && self.inner.objects.complete(&m)
            {
                m
            } else {
                let mut holders: Vec<NodeId> = vec![entry.producer];
                holders.extend(
                    object_sync::locate(&self.inner.engine, &self.inner.transport, me, &id, &hints)
                        .await,
                );
                holders.retain(|h| *h != me);
                holders.dedup();
                object_sync::fetch(
                    &self.inner.engine,
                    &self.inner.transport,
                    &self.inner.objects,
                    me,
                    job,
                    &id,
                    &holders,
                )
                .await?
            };
            let mut bytes = Vec::with_capacity(manifest.size as usize);
            for chunk in &manifest.chunks {
                match self.inner.objects.read_chunk(chunk) {
                    Some(data) => bytes.extend_from_slice(&data),
                    None => {
                        return Err(CoreError::Store(format!(
                            "cached object {} lost a chunk mid-read",
                            id.short()
                        )));
                    }
                }
            }
            out.push((id, bytes));
        }
        Ok(out)
    }
}
