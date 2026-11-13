//! History export: the sink table, the elected exporter, and the spool.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2). These methods call each
//! other and almost nothing else calls them, which is the other kind of
//! seam worth cutting along.

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// Every sink, with how far behind it is and what it last said
    /// (M31 B1 item 9).
    pub(super) fn sink_rows(&self) -> Vec<mesh_proto::SinkRow> {
        let me = self.node_id();
        let held = crate::sinks::paused_by_restore(&self.inner.config.layout().sinks_dir());
        // Records this node dropped despite a gating sink being behind
        // (§19 Q3). Read from the current plan rather than kept as a
        // counter, so it reflects what a pass would do right now.
        let forced = self
            .inner
            .engine
            .compaction_plan(self.inner.config.state.horizon_secs())
            .map(|p| p.forced_gaps)
            .unwrap_or(0);
        let founder = self.inner.engine.genesis().body.founder_user;
        let sinks_dir = self.inner.config.layout().sinks_dir();
        // The newest terminal row this node knows about, which is the top
        // of the clock lag is measured against.
        let newest = self
            .inner
            .history
            .as_ref()
            .and_then(|h| h.stats(&self.inner.config.layout().history()).ok())
            .and_then(|s| s.newest_micros)
            .unwrap_or(0);

        self.inner.engine.read(|s| {
            s.sinks
                .values()
                .filter(|e| !e.removed)
                .map(|entry| {
                    let sink = &entry.sink;
                    let elected = crate::sinks::elected_exporter(
                        s,
                        sink,
                        |id| self.inner.gossip.liveness_of(id),
                        |id| {
                            if *id == me {
                                Some(mesh_proto::PROTOCOL_MINOR)
                            } else {
                                self.inner.transport.peer_minor(id)
                            }
                        },
                    );
                    let state = crate::sinks::SinkState::new(&sinks_dir, &sink.name);
                    let cursor = state.cursor();
                    let caps = sink.capabilities();
                    // Are the numbers below this sink's, or just this
                    // node's copy of nothing? The cursor and spool are
                    // local files on whichever node exports, so on every
                    // other node they read back pristine — which is how a
                    // sink failing every tick for an hour looks healthy
                    // from four machines out of five. `last_at_micros` is
                    // in the test because a node that exported this sink
                    // until the election moved still holds a real reading,
                    // and blanking it would throw away the diagnosis at
                    // exactly the moment somebody wants it.
                    let status_is_local = elected == Some(me) || cursor.last_at_micros > 0;
                    // Lag in the data's own clock: the newest terminal row
                    // minus the cursor. Comparable across sinks, and still
                    // meaningful when nothing has run for an hour — a sink
                    // on an idle mesh is not behind.
                    let behind = newest.saturating_sub(cursor.jobs_micros);
                    mesh_proto::SinkRow {
                        name: sink.name.clone(),
                        kind: sink.kind.as_str().to_string(),
                        target: if sink.url.trim().is_empty() {
                            match sink.kind {
                                // The path the writer will actually use, so
                                // "where is the file" is answered by the
                                // row rather than by reading the layout.
                                //
                                // Only when this node is the one writing,
                                // though. Resolving a blank path anywhere
                                // else prints *this* machine's data
                                // directory for a file that is opened on
                                // another — which is not a cosmetic slip,
                                // it is a wrong answer to the only question
                                // the column exists to answer.
                                mesh_proto::SinkKind::Ndjson => {
                                    match crate::sinks::ndjson_path_at(&sink.database) {
                                        Some(p) => p.display().to_string(),
                                        None if status_is_local => {
                                            crate::sinks::ndjson_path(sink, &state)
                                                .display()
                                                .to_string()
                                        }
                                        None => "(the exporting node's data directory)".into(),
                                    }
                                }
                                _ => sink.database.clone(),
                            }
                        } else {
                            sink.url.clone()
                        },
                        database: sink.database.clone(),
                        tables: sink
                            .tables
                            .names()
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        enabled: sink.enabled,
                        run_on: sink.run_on.clone(),
                        elected: elected
                            .map(|n| {
                                s.members
                                    .get(&n)
                                    .map(|m| m.info.name.clone())
                                    .unwrap_or_else(|| n.short())
                            })
                            .unwrap_or_default(),
                        elected_here: elected == Some(me),
                        status_is_local,
                        // Zero rather than a huge number on a sink that has
                        // never exported: "behind by the age of the mesh"
                        // is technically true and useless.
                        lag_secs: if cursor.jobs_micros == 0 {
                            0
                        } else {
                            behind / 1_000_000
                        },
                        sent: cursor.sent,
                        spooled_bytes: state.spool_bytes(),
                        dropped: cursor.dropped,
                        last_status: cursor.last_status.clone(),
                        last_at_micros: cursor.last_at_micros,
                        paused_by_restore: held,
                        payloads_sent: cursor.payloads_sent,
                        payloads_too_large: cursor.payloads_too_large,
                        hold_for_export_days: sink.on_failure.hold_for_export_days,
                        forced_gaps: forced,
                        archives: caps.archives,
                        browse_records: caps.browse_records,
                        replay_payloads: caps.replay_payloads,
                        created_by: crate::rbac::username_of(s, &founder, &sink.created_by),
                        created_at_micros: sink.created_at_micros,
                        description: sink.description.clone(),
                    }
                })
                .collect()
        })
    }

    // -- M31 B1: sinks ---------------------------------------------------

    /// Export records to every armed sink this node is elected for.
    ///
    /// **Nothing in this loop is reachable from an admission path.** That is
    /// C3 and it is the property the phase lives or dies on: a full
    /// warehouse must never queue a job. The loop reads the local history
    /// index — which nothing blocks on — and when a destination will not
    /// take its rows it fills a bounded spool, drops the oldest, and counts
    /// it.
    ///
    /// Election is recomputed every pass from this node's own view, so a
    /// dead exporter costs one gossip round and nothing else. There is no
    /// handover: the cursor is a property of the data, so whoever wins next
    /// reads it and continues.
    pub(super) fn spawn_sink_task(self) {
        // A node with no history index has nothing to export, and there is
        // no second source to fall back to — the index *is* the projection
        // a sink copies.
        let Some(history) = self.inner.history.clone() else {
            return;
        };
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let tick = Duration::from_secs(self.inner.config.integrations.export_interval_secs);
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = shutdown.changed() => return,
                    }
                    this.export_once(&history).await;
                }
            }
        });
        self.track(handle);
    }

    /// May this node send this sink's data where the record points?
    ///
    /// **Checked on the exporting node, regardless of what the signed
    /// record says** — that asymmetry is the whole value of the control. A
    /// signed sink is what the mesh agreed to; this is what the machine
    /// will actually do, and only the machine knows its own network. It is
    /// one function because every path that opens a connection on a sink's
    /// behalf has to ask, and three call sites that each re-derive it are
    /// three chances for the fourth to be written without one.
    ///
    /// `None` means go; `Some` is the sentence to print.
    pub(super) fn egress_refusal(&self, sink: &mesh_proto::Sink) -> Option<String> {
        // `leaves_the_machine`, not `is_http` and not `url.is_empty()`.
        // A Postgres sink opens a socket to somebody else's server without
        // speaking HTTP, and a governance control that only knew about HTTP
        // would wave it through — which is the shape of the hole B3 found
        // in the payload path, arriving from a different direction.
        if !sink.kind.leaves_the_machine() || sink.url.trim().is_empty() {
            return None;
        }
        let host = url_host(&sink.url);
        (!self.inner.config.integrations.allows_host(&host))
            .then(|| self.inner.config.integrations.refusal_for(&host))
    }

    /// This mesh's settle window, in micros. See [`SINK_SETTLE_SECS`].
    pub(super) fn settle_micros(&self) -> u64 {
        self.inner.config.integrations.settle_secs * 1_000_000
    }

    /// One export pass over every sink this node is elected for.
    pub(super) async fn export_once(&self, history: &Arc<crate::history::HistoryStore>) {
        // §19 Q5: this node's data came out of a backup, so nothing exports
        // until an admin has looked at the list. Checked here rather than
        // at startup so the latch can be cleared without a restart.
        if crate::sinks::paused_by_restore(&self.inner.config.layout().sinks_dir()) {
            return;
        }
        let me = self.node_id();
        let armed: Vec<mesh_proto::Sink> = self.inner.engine.read(|s| {
            s.sinks
                .values()
                .filter(|e| e.is_armed())
                .filter(|e| {
                    crate::sinks::elected_exporter(
                        s,
                        &e.sink,
                        |id| self.inner.gossip.liveness_of(id),
                        |id| {
                            // This node knows its own minor without a
                            // negotiation; every peer's comes from the
                            // handshake.
                            if *id == me {
                                Some(mesh_proto::PROTOCOL_MINOR)
                            } else {
                                self.inner.transport.peer_minor(id)
                            }
                        },
                    ) == Some(me)
                })
                .map(|e| e.sink.clone())
                .collect()
        });
        for sink in armed {
            if let Err(e) = self.export_sink(&sink, history).await {
                // Never fatal, never retried in place: the next tick is the
                // retry, and the spool is what holds the rows meanwhile.
                warn!(sink = %sink.name, error = %e, "sink export pass failed");
            }
        }

        // M31 B2 item 5. **A separate pass, on every node, for its own
        // jobs** — not only the elected one. Records are small and ordered
        // and want one cursor from one machine; raw stdout is megabytes and
        // already sits on the producer, and routing it through the elected
        // node would make a thousand-node mesh export through one NIC.
        let with_payloads: Vec<mesh_proto::Sink> = self.inner.engine.read(|s| {
            s.sinks
                .values()
                .filter(|e| e.is_armed() && e.sink.payloads.enabled)
                .map(|e| e.sink.clone())
                .collect()
        });
        for sink in with_payloads {
            if let Err(e) = self.export_payloads(&sink, history).await {
                warn!(sink = %sink.name, error = %e, "payload export pass failed");
            }
        }
    }

    /// Export this node's own job output to a sink that takes payloads.
    ///
    /// Opt-in (`payloads.enabled`), capped (`payloads.max_bytes`), and from
    /// the producer only. Above the cap the **reference** is recorded and
    /// the bytes stay here — silently truncating would be worse than
    /// refusing, because nobody would know which half they had.
    pub(super) async fn export_payloads(
        &self,
        sink: &mesh_proto::Sink,
        history: &Arc<crate::history::HistoryStore>,
    ) -> crate::error::Result<()> {
        use crate::sinks::{Mark, SinkState};

        if !sink.kind.is_http() || !sink.kind.takes_payloads() {
            // A file or a plugin sink already receives whatever the record
            // pass writes; there is no second channel to send bytes down,
            // and inventing one would be a payload path per kind. And two
            // destinations refuse payloads outright whatever the operator
            // set — see `SinkKind::takes_payloads`: a payload line is not a
            // span, and it is not a row in a published schema either.
            return Ok(());
        }
        // **The same door.** This path shipped without the check, which is
        // exactly backwards: raw stdout is the most sensitive thing a sink
        // ever carries, and it was the one path that left without asking.
        // Hence [`Self::egress_refusal`] — one function, so a fourth caller
        // cannot forget in the same way.
        if let Some(why) = self.egress_refusal(sink) {
            warn!(sink = %sink.name, "{why}");
            return Ok(());
        }
        let me = self.node_id();
        let state = SinkState::new(&self.inner.config.layout().sinks_dir(), &sink.name);
        let mut cursor = state.cursor();
        let mesh_id = self.inner.engine.genesis().body.mesh_id.to_display_string();
        let cutoff = mesh_proto::now_micros().saturating_sub(self.settle_micros());

        // This node's own terminal jobs, oldest first past the cursor.
        let page = history.query_jobs(&mesh_proto::HistoryQuery {
            node: Some(me),
            since_micros: Some(cursor.payload_micros),
            until_micros: Some(cutoff),
            limit: PAYLOAD_BATCH,
            ..Default::default()
        })?;
        let mut rows = page.rows;
        rows.sort_by_key(|r| (r.sort_micros(), r.job_id.to_display_string()));

        let mark = cursor.payload_mark();
        let credential = self.sink_credential(sink).await.unwrap_or_default();
        // Locators earned this pass, written in one transaction below
        // rather than one fsync per exported job (**L-4**).
        let mut locators: Vec<(mesh_proto::JobId, String, String)> = Vec::new();
        // Payload lines accumulated across jobs, POSTed together (**H-6**),
        // with the jobs they belong to so the cursor and the locators can
        // be settled for the whole batch once it lands.
        let mut pending: Vec<serde_json::Value> = Vec::new();
        let mut pending_jobs: Vec<(mesh_proto::JobId, crate::sinks::Mark)> = Vec::new();
        // Running total for `pending`, so the size guard stays O(1) per job.
        let mut pending_bytes = 0usize;
        let mut advanced = false;

        for row in &rows {
            let Some(finished) = row.finished_at_micros.filter(|f| *f > 0) else {
                continue;
            };
            let here = Mark::new(finished, row.job_id.to_display_string());
            if here <= mark {
                continue;
            }
            match crate::sinks::read_payload(&self.inner.jobs, &row.job_id, sink.payloads.max_bytes)
            {
                // Over the cap: the reference goes, the bytes do not, and
                // the operator can see the count.
                Some(Err(_)) => {
                    cursor.payloads_too_large += 1;
                    cursor.set_payload(here);
                    advanced = true;
                }
                // Nothing captured, or already pruned by `[retention]`
                // before the sink got to it. **Not an error** — A2's rule,
                // inherited rather than reinvented: the record still says
                // what happened, and the payload reads "expired — not
                // exported".
                None => {
                    cursor.set_payload(here);
                    advanced = true;
                }
                Some(Ok((out, err))) => {
                    let mut lines =
                        crate::sinks::payload_lines(&mesh_id, &row.job_id, "stdout", &out);
                    lines.extend(crate::sinks::payload_lines(
                        &mesh_id,
                        &row.job_id,
                        "stderr",
                        &err,
                    ));
                    if lines.is_empty() {
                        cursor.set_payload(here);
                        advanced = true;
                        continue;
                    }
                    // **One POST per job was one TLS handshake per job**
                    // (**H-6**). A full tick is `PAYLOAD_BATCH` (100) of
                    // them, each paying DNS, TCP and a TLS handshake to a
                    // remote warehouse — 20–100 ms apiece — so a node with
                    // two HTTP sinks spent seconds per tick shaking hands.
                    // The record path next door has always batched; this is
                    // the same batching, with a byte cap so one chatty job
                    // cannot make the request unbounded.
                    pending_bytes += payload_bytes(&lines);
                    pending.extend(lines);
                    pending_jobs.push((row.job_id, here));
                    if pending_bytes < PAYLOAD_POST_MAX {
                        continue;
                    }
                    pending_bytes = 0;
                    if !flush_payloads(
                        sink,
                        &credential,
                        &mesh_id,
                        &mut pending,
                        &mut pending_jobs,
                        &mut cursor,
                        &mut locators,
                        &mut advanced,
                    )
                    .await
                    {
                        // Stop at the first refusal rather than walking the
                        // whole page into a wall: the next tick retries,
                        // and the cursor has not moved past what failed.
                        break;
                    }
                }
            }
        }

        // The tail of the batch (**H-6**).
        let _ = flush_payloads(
            sink,
            &credential,
            &mesh_id,
            &mut pending,
            &mut pending_jobs,
            &mut cursor,
            &mut locators,
            &mut advanced,
        )
        .await;
        // One transaction for every locator this pass earned (**L-4**).
        if !locators.is_empty() {
            let _ = history.set_payload_locators(&locators);
        }
        if advanced {
            state.save_cursor(&cursor)?;
        }
        Ok(())
    }

    /// Export one sink's next batch.
    pub(super) async fn export_sink(
        &self,
        sink: &mesh_proto::Sink,
        history: &Arc<crate::history::HistoryStore>,
    ) -> crate::error::Result<()> {
        use crate::sinks::{Mark, SinkState, WriteOutcome, encode_batch, job_line};

        if let Some(why) = self.egress_refusal(sink) {
            warn!(sink = %sink.name, "{why}");
            return Ok(());
        }

        let state = SinkState::new(&self.inner.config.layout().sinks_dir(), &sink.name);
        let mut cursor = state.cursor();
        // **Pick up where the last exporter left off, not from zero.**
        //
        // The cursor is a local file, so a node that has just won this
        // sink's election has never written one and reads back a fresh
        // zero. Left alone that means every handover re-exports the whole
        // history: correct, because consumers deduplicate on `run_id`, and
        // ruinous, because "correct" here is a full re-ingest of somebody's
        // warehouse every time a machine reboots.
        //
        // `sink_progress` is the watermark the previous exporter published
        // into replicated state, so it is already on this disk and needs no
        // handover protocol — which is what `spawn_sink_task`'s comment
        // means by the cursor being a property of the data rather than of
        // the node.
        //
        // **Only when the destination outlives this node's own disk**, and
        // the two exceptions are the whole subtlety:
        //
        // - A **plugin** hands batches to a program on *this* machine, so
        //   another node's progress says nothing about what this one has
        //   delivered.
        // - A **blank-path file sink** writes into `<home>/sinks/<name>/` —
        //   the very directory the cursor sits in. A fresh cursor there
        //   means a fresh, absent file, so seeding would skip straight past
        //   history that this node's file has never held. That is also the
        //   shape of every sink created before 1.19 gated them, which is
        //   exactly the population that must not silently lose rows to a
        //   feature it never opted into.
        //
        // A file sink with an explicit path is safe because 1.19 refuses to
        // create one that does not resolve to a single file, so the file
        // the watermark describes is the file this node is about to open.
        let destination_outlives_this_node = match sink.kind {
            mesh_proto::SinkKind::Plugin => false,
            mesh_proto::SinkKind::Ndjson => !sink.database.trim().is_empty(),
            _ => true,
        };
        if cursor.last_at_micros == 0 && destination_outlives_this_node {
            let published = self
                .inner
                .engine
                .read(|s| s.sink_progress.get(&sink.name).copied().unwrap_or(0));
            if published > 0 {
                cursor.seed_from(published);
                debug!(
                    sink = %sink.name, through = published,
                    "took over a sink; continuing from the published watermark"
                );
            }
        }
        let mesh_id = self.inner.engine.genesis().body.mesh_id.to_display_string();

        // Terminal rows only, and only past the settle lag — the archive's
        // rule, for the archive's reason: a run that finished on another
        // node reaches this one whenever gossip gets round to it, and
        // exporting five minutes behind the present makes "the row arrived
        // late" a non-event rather than a permanent gap in somebody's
        // warehouse.
        let cutoff = mesh_proto::now_micros().saturating_sub(self.settle_micros());
        let limit = sink.records.batch.min(mesh_proto::HistoryQuery::MAX_LIMIT);

        let mut lines: Vec<serde_json::Value> = Vec::new();
        let mut next_jobs: Option<Mark> = None;
        let mut next_usage: Option<Mark> = None;
        let mut next_audit: Option<Mark> = None;

        if sink.tables.jobs {
            let page = history.query_jobs(&mesh_proto::HistoryQuery {
                since_micros: Some(cursor.jobs_micros),
                until_micros: Some(cutoff),
                limit,
                ..Default::default()
            })?;
            let mut rows = page.rows;
            rows.sort_by_key(|r| (r.sort_micros(), r.job_id.to_display_string()));
            let mark = cursor.jobs_mark();
            for row in &rows {
                let Some(finished) = row.finished_at_micros.filter(|f| *f > 0) else {
                    continue;
                };
                let here = Mark::new(finished, row.job_id.to_display_string());
                if here <= mark {
                    continue;
                }
                lines.push(job_line(&mesh_id, row));
                next_jobs = Some(here);
            }
        }

        // Usage windows (protocol 1.19). **Closed windows only**, which is
        // the one thing this pass has to get right that the record passes
        // do not: a usage row is an upsert, not an append, so a window that
        // is still accumulating would be exported now and contradicted on
        // the next pass — and no consumer deduplicating on a key can tell
        // which of the two was final. `usage_window_end_micros` past the
        // settle cutoff is exactly "this window can no longer change".
        if sink.tables.usage {
            let rows = history.usage_page(cursor.usage_micros, None, None, limit)?;
            let mark = cursor.usage_mark();
            let mut sorted = rows;
            sorted.sort_by_key(|r| (r.at_micros(), r.node.to_display_string()));
            for row in &sorted {
                if mesh_proto::usage_window_end_micros(row.window) > cutoff {
                    continue;
                }
                let here = Mark::new(
                    row.at_micros(),
                    format!(
                        "{}:{}",
                        row.node.to_display_string(),
                        row.principal.to_display_string()
                    ),
                );
                if here <= mark {
                    continue;
                }
                lines.push(crate::sinks::usage_line(&mesh_id, row));
                next_usage = Some(here);
            }
        }

        // The audit trail (protocol 1.19), with the signatures that make it
        // worth exporting at all — see `audit_line`.
        //
        // Read from the op-log rather than from materialized state, which
        // costs a full scan; `signed_audit_rows` explains why that is the
        // only place the proofs exist. It is affordable here for the reason
        // it is affordable for the archive: this runs on a timer, off every
        // interactive path, and only for a sink whose operator ticked a box
        // that says the audit log leaves the mesh.
        //
        // On a blocking thread, though (ENG-20). A timer is not a licence
        // to walk the whole op-log on a reactor worker every thirty
        // seconds: the scan itself is bounded, but the core it parks is not
        // the sink's to spend.
        if sink.tables.audit {
            let engine = self.inner.engine.clone();
            let since = cursor.audit_micros;
            let rows = tokio::task::spawn_blocking(move || engine.signed_audit_rows(since, limit))
                .await
                .map_err(|e| CoreError::Failed(format!("audit scan task failed: {e}")))??;
            let mark = cursor.audit_mark();
            for row in &rows {
                if row.entry.at_micros > cutoff {
                    continue;
                }
                let here = Mark::new(row.entry.at_micros, row.entry.audit_id.to_display_string());
                if here <= mark {
                    continue;
                }
                lines.push(crate::sinks::audit_line(&mesh_id, row));
                next_audit = Some(here);
            }
        }

        if lines.is_empty() {
            return Ok(());
        }
        let rows = lines.len();
        let batch = encode_batch(&lines);

        // M31 B2. An HTTP destination needs the decoded rows — to name a
        // ClickHouse table, or to build a `_bulk` action line — and a
        // credential resolved *here*, so the two paths diverge only at the
        // write itself. Everything either side of this (the cursor, the
        // spool, the drop-and-count) is shared, which is the point of
        // "four named destinations, one code path".
        let outcome = if sink.kind.is_http() {
            match self.sink_credential(sink).await {
                Ok(credential) => crate::sinks::http_write(sink, &lines, &credential).await,
                // A missing or unreadable secret is a sink failure like any
                // other: it spools and retries, because the secret may
                // simply not have replicated to this node yet.
                Err(e) => crate::sinks::WriteOutcome::Failed {
                    status: format!("credential: {e}"),
                },
            }
        } else {
            // Postgres needs its password here too, and gets it the same
            // way: resolved on this node, at connect time, into the
            // subprocess's environment rather than its argv.
            let _credential = self.sink_credential(sink).await.unwrap_or_default();
            crate::sinks::write_batch(sink, &state, &batch, rows).await
        };

        match outcome {
            WriteOutcome::Accepted => {
                // A 2xx — or a plugin's ack — is the only thing that
                // advances the cursor.
                if let Some(m) = next_jobs {
                    cursor.set_jobs(m);
                }
                if let Some(m) = next_usage {
                    cursor.set_usage(m);
                }
                if let Some(m) = next_audit {
                    cursor.set_audit(m);
                }
                cursor.sent += rows as u64;
                cursor.last_status = "ok".into();
                cursor.last_at_micros = mesh_proto::now_micros();
                state.save_cursor(&cursor)?;
                // §19 Q3. Publish how far we have got, so **every** node's
                // compactor can honour the gate — the cursor above is on
                // one disk, and compaction happens everywhere. Authored
                // only when it moves, and LWW per sink, so it collapses to
                // one live op per sink however long the mesh runs.
                // Publish how far we have got — **for every sink, not only
                // a gating one** (protocol 1.19).
                //
                // It began as the compaction gate's input, which is why it
                // used to be written only when `hold_for_export_days > 0`.
                // It is now also what a node picks up when it wins this
                // sink's election, and a watermark that exists only for
                // gating sinks would mean every other sink still re-exports
                // its whole history on handover. LWW per sink, authored only
                // when it moves, so this stays one live op per sink however
                // long the mesh runs.
                {
                    // **The minimum across the tables this sink actually
                    // exports**, not across all of them.
                    //
                    // A blanket `jobs.min(usage)` reads zero forever on
                    // a sink that exports only one of them — its unused
                    // cursor never leaves the floor — so the watermark
                    // never advances, `export_gate` pins compaction at the
                    // beginning of time, and a gating sink silently holds
                    // the op-log open for good.
                    let mut through = u64::MAX;
                    for enabled in [
                        sink.tables.jobs.then_some(cursor.jobs_micros),
                        sink.tables.usage.then_some(cursor.usage_micros),
                        sink.tables.audit.then_some(cursor.audit_micros),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        through = through.min(enabled);
                    }
                    let through = if through == u64::MAX { 0 } else { through };
                    let published = self
                        .inner
                        .engine
                        .read(|s| s.sink_progress.get(&sink.name).copied().unwrap_or(0));
                    if through > published {
                        let _ = self
                            .inner
                            .engine
                            .author_op(mesh_proto::OpBody::SinkProgress {
                                sink: sink.name.clone(),
                                exported_through_micros: through,
                            });
                    }
                }
                // Anything spooled while the destination was down goes now,
                // oldest first, before the next fresh batch.
                self.drain_spool(sink, &state).await;
            }
            WriteOutcome::PartiallyAccepted { rows: acked } => {
                // The cursor advances **only to the acked row**. Advancing
                // past what was accepted would be silent data loss dressed
                // as progress, which is the one failure a sink must not
                // have.
                warn!(
                    sink = %sink.name,
                    acked, offered = rows,
                    "sink accepted only part of a batch; the rest is offered again"
                );
                cursor.sent += acked as u64;
                cursor.last_status = format!("partial: {acked}/{rows}");
                cursor.last_at_micros = mesh_proto::now_micros();
                state.save_cursor(&cursor)?;
            }
            WriteOutcome::Failed { status } => {
                let dropped = state.spool(&batch, sink.on_failure.spool_bytes)?;
                cursor.dropped += dropped;
                cursor.last_status = status.clone();
                cursor.last_at_micros = mesh_proto::now_micros();
                state.save_cursor(&cursor)?;
                if dropped > 0 {
                    // Loud, and it says how much: a sink that is dropping
                    // is a sink whose destination has been down long
                    // enough to matter, and the operator has to be able to
                    // tell that from "it is slow".
                    warn!(
                        sink = %sink.name, dropped, status,
                        "sink spool is full — dropping the oldest batches"
                    );
                } else {
                    debug!(sink = %sink.name, status, "sink write failed; spooled");
                }
            }
        }
        Ok(())
    }

    // -- M31 B3 §14.2: one History list that spans the tiers -------------
    //
    // `history.redb` is instant and recent; `archive/` is complete and
    // slower. Those are properties of a *result*, not a reason for two
    // screens — so a browse answers from the index, and continues into
    // Parquet only when the index has run out of rows to give.
    //
    // **The fence (§18, and this phase's most important test): none of this
    // ever touches a sink.** A list, its filters and its counts are answered
    // from tiers this mesh owns. A remote destination is read on one
    // person's click on one payload, never to paint a page.

    /// How many peers a browse may ask for an archive page before it gives
    /// up and says so.
    ///
    /// Three. A browse must not fan out across a fifty-node mesh — the
    /// point of the archive is that *somebody* has one, and if the first
    /// three live peers do not, saying "no archive answered" is both true
    /// and cheap. The alternative is a History page whose latency is a
    /// function of mesh size.
    const ARCHIVE_PEERS_TRIED: usize = 3;

    /// Ask for one archive page: this node's Parquet if it has any, else a
    /// peer's (§19 Q11, decided as **"ask an archive-holding peer"**).
    ///
    /// Returns the rows, who answered, and — when nobody did — why, so the
    /// caller can render a known absence rather than an empty list. The two
    /// are different facts and a UI that conflates them reports data loss.
    pub(super) async fn archive_page(
        &self,
        q: &mesh_proto::HistoryQuery,
    ) -> (mesh_proto::ArchivePageReply, String, String) {
        let dir = self.inner.config.layout().archive_dir();

        // Ours, if we have one. No election and no network: the cheapest
        // correct answer is the local one.
        if crate::archive_read::has_archive(&dir) {
            return match crate::archive_read::query(&dir, q).await {
                Ok(rows) => (rows_into_reply(&rows, true), String::new(), String::new()),
                Err(e) => (
                    mesh_proto::ArchivePageReply {
                        jobs: Vec::new(),
                        // We *have* an archive; it failed to answer. Saying
                        // "no archive" here would blame the wrong thing.
                        has_archive: true,
                    },
                    String::new(),
                    e.to_string(),
                ),
            };
        }

        // Somebody else's. Live members only, capable ones only, in a
        // deterministic order so paging is stable — a browse that asked a
        // different peer per page would interleave two archives.
        let me = self.node_id();
        let mut peers: Vec<NodeId> = self.inner.engine.read(|s| {
            s.members
                .values()
                .map(|m| m.info.node_id)
                .filter(|id| *id != me)
                .filter(|id| matches!(self.inner.gossip.liveness_of(id), Liveness::Alive))
                .collect()
        });
        peers.sort_by_key(|id| id.0);

        let mut tried = 0usize;
        let mut last_error = String::new();
        for node in peers {
            if tried >= Self::ARCHIVE_PEERS_TRIED {
                break;
            }
            if self.inner.transport.peer_minor(&node).unwrap_or(0) < mesh_proto::ARCHIVE_MINOR {
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
            tried += 1;
            let request = Request::ArchivePage {
                query: Box::new(q.clone()),
            };
            match self.inner.transport.request_one(&conn, &request).await {
                Ok(Response::ArchivePage(reply)) => {
                    if reply.has_archive {
                        return (*reply, self.member_name(&node), String::new());
                    }
                }
                Ok(_) => {}
                Err(e) => last_error = format!("{}: {e}", self.member_name(&node)),
            }
        }
        (
            mesh_proto::ArchivePageReply {
                jobs: Vec::new(),
                has_archive: false,
            },
            String::new(),
            last_error,
        )
    }

    /// What this node is refusing, and what is churning (M31 A3).
    pub(super) fn backpressure_view(&self) -> mesh_proto::BackpressureView {
        let now = mesh_proto::now_micros();
        let guard = self.inner.engine.rate_guard();
        mesh_proto::BackpressureView {
            limit_per_minute: guard.limit(),
            entities: guard
                .pressured(now)
                .into_iter()
                .map(|p| mesh_proto::PressureRow {
                    entity: p.entity,
                    observed_per_minute: p.observed_per_minute,
                    refused_total: p.refused_total,
                    paused_until_micros: p.paused_until_micros,
                })
                .collect(),
            // The same question, asked of the one entity kind that had an
            // answer before A3 generalised it.
            flapping: self
                .inner
                .flap
                .flapping()
                .into_iter()
                .map(|(key, _)| key)
                .collect(),
        }
    }

    /// Fill in where each row's output is (§14.3 item 7's fourth state).
    ///
    /// One state read for the whole page rather than one per row, and one
    /// pass: the availability is a fact about *now*, which is exactly why
    /// it is computed at answer time and never stored.
    ///
    /// **A row the op-log no longer knows about is not "available".** A1
    /// pruned it months ago; what survives is the index row, and the only
    /// honest readings are "the bytes are in that destination" when a
    /// locator says so, and "expired" when nothing does. The destination's
    /// mere existence never softens the second into the first.
    pub(super) fn fill_availability(&self, rows: &mut [mesh_proto::HistoryJobRow]) {
        use mesh_proto::{ExpiryReason, OutputAvailability};
        let me = self.node_id();
        let days = u64::from(self.resolved_retention().job_days);
        self.inner.engine.read(|state| {
            for row in rows.iter_mut() {
                row.output = Some(match state.jobs.get(&row.job_id) {
                    Some(summary) => self.output_availability_in(state, summary, me),
                    None => match (&row.payload_sink, &row.payload_key) {
                        (Some(sink), Some(key)) => OutputAvailability::Elsewhere {
                            sink: sink.clone(),
                            key: key.clone(),
                        },
                        _ => OutputAvailability::Expired {
                            when_micros: row
                                .finished_at_micros
                                .unwrap_or(row.created_at_micros)
                                .saturating_add(days * 86_400 * 1_000_000),
                            reason: ExpiryReason::Retention,
                        },
                    },
                });
            }
        });
    }

    /// A job browse that spans the index and the archive.
    pub(super) async fn history_jobs_spanning(
        &self,
        h: &crate::history::HistoryStore,
        q: &mesh_proto::HistoryQuery,
    ) -> crate::error::Result<mesh_proto::HistoryJobPage> {
        // See `history_runs_spanning` for why this is not `q.limit`.
        let limit = q.effective_limit();
        let mut page = h.query_jobs(q)?;
        if page.rows.len() >= limit {
            self.fill_availability(&mut page.rows);
            return Ok(page);
        }
        let deep = deepen(q);
        let (reply, peer, status) = self.archive_page(&deep).await;
        if !reply.has_archive {
            self.fill_availability(&mut page.rows);
            return Ok(mesh_proto::HistoryJobPage {
                tiers: mesh_proto::TierReport {
                    from_index: page.rows.len() as u64,
                    archive_unreachable: true,
                    archive_status: status,
                    ..Default::default()
                },
                ..page
            });
        }
        let index_deep = h.query_jobs(&deep)?;
        let reply_len = reply.jobs.len();
        let index_rows = index_deep.rows.len();
        let mut merged: Vec<mesh_proto::HistoryJobRow> = index_deep.rows;
        let known: std::collections::HashSet<mesh_proto::JobId> =
            merged.iter().map(|r| r.job_id).collect();
        merged.extend(
            reply
                .jobs
                .into_iter()
                .filter(|r| !known.contains(&r.job_id)),
        );
        merged.sort_by(|a, b| {
            b.sort_micros()
                .cmp(&a.sort_micros())
                .then(b.job_id.0.cmp(&a.job_id.0))
        });
        let total = exact_total(
            merged.len(),
            index_rows,
            index_deep.total,
            reply_len,
            deep.limit,
        );
        let mut rows: Vec<_> = merged
            .into_iter()
            .skip(q.offset as usize)
            .take(limit)
            .collect();
        let from_archive = rows.iter().filter(|r| r.from_archive).count() as u64;
        self.fill_availability(&mut rows);
        Ok(mesh_proto::HistoryJobPage {
            total,
            tiers: mesh_proto::TierReport {
                from_index: rows.len() as u64 - from_archive,
                from_archive,
                archive_peer: peer,
                archive_unreachable: false,
                archive_status: status,
            },
            absence: mesh_proto::KnownAbsence::default(),
            rows,
        })
    }

    /// One window of one job's exported output, fetched by whichever node
    /// can actually reach the destination (M31 B3 §14.3).
    ///
    /// **A failed read degrades one panel and never breaks a screen.** Every
    /// return below is a `PayloadWindowReply` carrying prose, not an error —
    /// that is Q7's degrade-versus-break test, decided, and the reason this
    /// returns a reply rather than a `Result`.
    pub(super) async fn replay_payload(
        &self,
        job_id: &mesh_proto::JobId,
        from_end: u64,
        limit: u64,
        search: &str,
        env: &SignedIpc,
        user: &UserId,
    ) -> mesh_proto::PayloadWindowReply {
        let refuse = |status: String| mesh_proto::PayloadWindowReply {
            lines: Vec::new(),
            more_before: false,
            status,
        };

        // The locator, from the index. Never from the caller — see the
        // request's own doc comment for why.
        let Some((sink_name, key)) = self.payload_locator(job_id) else {
            return refuse(
                "this job's output was never exported to a destination, so there is nothing \
                 to read back — the record still says what happened"
                    .to_string(),
            );
        };
        let sink = self.inner.engine.read(|s| {
            s.sinks
                .get(&sink_name)
                .filter(|e| !e.removed)
                .map(|e| e.sink.clone())
        });
        let Some(sink) = sink else {
            return refuse(format!(
                "this mesh has no destination named `{sink_name}` any more — the record says \
                 the output went there, and nothing here can reach it"
            ));
        };

        // **One audit entry per open.** The tail read is the open; a scroll
        // back writes none. Recorded before the fetch, so an attempt that
        // fails is still on the record — the interesting audit question is
        // who asked, not whether the warehouse was up.
        if from_end == 0 {
            self.audit_ok(
                user,
                "history.replay",
                &job_id.to_display_string(),
                &format!("sink={} host={}", sink.name, url_host(&sink.url)),
                env,
            );
        }

        // The node that exported it is the node that fetches it — the same
        // election as the write path, so the machine with the route and the
        // credential is the one that opens the connection.
        let me = self.node_id();
        let elected = self.inner.engine.read(|s| {
            crate::sinks::elected_exporter(
                s,
                &sink,
                |id| self.inner.gossip.liveness_of(id),
                |id| {
                    if *id == me {
                        Some(mesh_proto::PROTOCOL_MINOR)
                    } else {
                        self.inner.transport.peer_minor(id)
                    }
                },
            )
        });
        let window = crate::sinks::PayloadWindow {
            from_end,
            // Clamped here as well as on the fetching node: a caller that
            // asked for a million lines gets a page, not an out-of-memory.
            limit: limit.clamp(1, 5_000),
            search: search.to_string(),
        };
        let page = match elected {
            Some(node) if node != me => {
                match self
                    .replay_payload_on(node, job_id, &sink.name, &key, &window)
                    .await
                {
                    Ok(reply) => return reply,
                    Err(status) => return refuse(status),
                }
            }
            // Ours, or nobody's. "Nobody's" still tries here: a single-node
            // mesh whose peer list has not settled should read its own
            // warehouse rather than refuse to.
            _ => {
                self.replay_payload_locally(job_id, &sink.name, &key, &window)
                    .await
            }
        };
        mesh_proto::PayloadWindowReply {
            lines: page.lines,
            more_before: page.more_before,
            status: page.status,
        }
    }

    /// Ask the elected node for the window.
    ///
    /// Errors come back as prose for the panel rather than as failures: a
    /// warehouse this node cannot reach is a fact about the read, not a
    /// broken History screen.
    pub(super) async fn replay_payload_on(
        &self,
        node: NodeId,
        job_id: &mesh_proto::JobId,
        sink: &str,
        key: &str,
        window: &crate::sinks::PayloadWindow,
    ) -> std::result::Result<mesh_proto::PayloadWindowReply, String> {
        if self.inner.transport.peer_minor(&node).unwrap_or(0) < mesh_proto::ARCHIVE_MINOR {
            return Err(format!(
                "`{}` holds the route to this destination but is running an older meshware \
                 that cannot read output back",
                self.member_name(&node)
            ));
        }
        let addrs = self
            .inner
            .engine
            .read(|s| s.members.get(&node).map(|m| m.info.addrs.clone()))
            .unwrap_or_default();
        let conn = self
            .inner
            .transport
            .member_conn(node, &addrs)
            .await
            .map_err(|e| format!("could not reach `{}`: {e}", self.member_name(&node)))?;
        let request = Request::ReplayPayload {
            job_id: *job_id,
            sink: sink.to_string(),
            key: key.to_string(),
            from_end: window.from_end,
            limit: window.limit,
            search: window.search.clone(),
        };
        match self.inner.transport.request_one(&conn, &request).await {
            Ok(Response::PayloadWindow(reply)) => Ok(*reply),
            Ok(other) => Err(format!(
                "`{}` answered oddly: {other:?}",
                self.member_name(&node)
            )),
            Err(e) => Err(format!("`{}` did not answer: {e}", self.member_name(&node))),
        }
    }

    /// Fetch one window of a job's exported output, from this node
    /// (M31 B3 §14.3).
    ///
    /// Everything the read has to satisfy, in one place:
    ///
    /// - **`egress_allow`, exactly as a write is.** A read leaves by the
    ///   same door, so it is governed by the same rule — checked here on
    ///   the fetching node, regardless of what any signed record says.
    /// - **The read credential, and no fallback** (§19 Q10, decided).
    ///   Absent, the read is refused and the message names the permission
    ///   to grant. Reusing the write token would quietly widen a grant an
    ///   admin deliberately scoped, and the operator would never know.
    /// - **A window, never the object.** Nothing here materialises 5 GB;
    ///   the destination is asked for a page in its own idiom.
    pub(super) async fn replay_payload_locally(
        &self,
        job_id: &mesh_proto::JobId,
        sink_name: &str,
        key: &str,
        window: &crate::sinks::PayloadWindow,
    ) -> crate::sinks::PayloadPage {
        use crate::sinks::PayloadPage;

        let sink = self.inner.engine.read(|s| {
            s.sinks
                .get(sink_name)
                .filter(|e| !e.removed)
                .map(|e| e.sink.clone())
        });
        let Some(sink) = sink else {
            return PayloadPage {
                lines: Vec::new(),
                more_before: false,
                status: format!(
                    "this mesh has no destination named `{sink_name}` any more — the record \
                     says the output went there, and nothing here can reach it"
                ),
            };
        };

        // A read leaves by the same door as a write.
        if let Some(why) = self.egress_refusal(&sink) {
            return PayloadPage {
                lines: Vec::new(),
                more_before: false,
                status: why,
            };
        }

        // §19 Q10: refuse, and say what is missing.
        let state = sink
            .kind
            .replay_state(!sink.read_credential.trim().is_empty());
        if state != mesh_proto::ReplayState::Available {
            return PayloadPage {
                lines: Vec::new(),
                more_before: false,
                status: state.message().to_string(),
            };
        }
        let credential = match self
            .resolve_reference(&sink.read_credential, sink_name)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return PayloadPage {
                    lines: Vec::new(),
                    more_before: false,
                    status: format!("read credential: {e}"),
                };
            }
        };

        let _ = job_id; // the locator already names the row; kept for audit callers
        crate::sinks::read_payload_window(
            &sink,
            key,
            &crate::sinks::PayloadWindow {
                limit: window.limit.clamp(1, 5_000),
                ..window.clone()
            },
            &credential,
        )
        .await
    }

    /// Resolve a sink's `{{secret:…}}` credential — **on this node, at
    /// connect time** (C6).
    ///
    /// The reference travels in the signed record; the value never does. So
    /// the token is not in the op-log, not in Director's memory, and not in
    /// any process's argv — it exists for the length of one POST, on the
    /// one machine that actually opens the connection.
    ///
    /// An empty credential is not an error: a file sink needs none, and a
    /// destination behind a network boundary may need none either.
    pub(super) async fn sink_credential(
        &self,
        sink: &mesh_proto::Sink,
    ) -> crate::error::Result<String> {
        self.resolve_reference(&sink.credential, &sink.name).await
    }

    /// One `{{secret:…}}` reference, resolved on this node.
    ///
    /// Shared by the write credential and B3's read credential, so there is
    /// one place a sink token is ever unwrapped rather than two that could
    /// diverge in how carefully they do it.
    pub(super) async fn resolve_reference(
        &self,
        reference: &str,
        sink_name: &str,
    ) -> crate::error::Result<String> {
        if reference.trim().is_empty() {
            return Ok(String::new());
        }
        let providers = self.live_config().secret_providers.clone();
        let dir = self
            .inner
            .config
            .layout()
            .sinks_dir()
            .join(sink_name)
            .join("secrets");
        let resolved = crate::secrets::resolve_env(
            &self.inner.engine,
            self.node_key_for_test(),
            &providers,
            vec![("CREDENTIAL".to_string(), reference.to_string())],
            &dir,
            // The node's own use: an admin configured this sink.
            None,
        )
        .await?;
        // `resolve_env` writes `{{secret_file:…}}` values to disk and
        // returns a path; a sink credential is a header value, so only the
        // inline form is meaningful here.
        let value = resolved
            .env
            .into_iter()
            .find(|(k, _)| k == "CREDENTIAL")
            .map(|(_, v)| v)
            .unwrap_or_default();
        Ok(value)
    }

    /// Send what the spool is holding, oldest first.
    ///
    /// Only after a live batch succeeded, so a destination that is still
    /// down is not hammered with a backlog on every tick.
    pub(super) async fn drain_spool(
        &self,
        sink: &mesh_proto::Sink,
        state: &crate::sinks::SinkState,
    ) {
        for path in state.spooled() {
            let Ok(batch) = std::fs::read(&path) else {
                continue;
            };
            let rows = batch.iter().filter(|b| **b == b'\n').count();
            let _credential = self.sink_credential(sink).await.unwrap_or_default();
            match crate::sinks::write_batch(sink, state, &batch, rows).await {
                crate::sinks::WriteOutcome::Accepted => {
                    let _ = std::fs::remove_file(&path);
                }
                // Still down, or down again. Stop rather than walking the
                // whole spool into a wall — the next tick tries again.
                _ => break,
            }
        }
    }
}
