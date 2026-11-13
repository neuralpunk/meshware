//! `GET /healthz` and `GET /metrics`: this node's own view of itself.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// `GET /healthz` body: liveness plus the vitals a probe cares about.
    pub fn healthz_json(&self) -> String {
        self.healthz().1
    }

    /// `GET /healthz`: whether this node is working, and the body.
    ///
    /// Unhealthy while a critical loop has ended or stalled (ENG-2), with
    /// what is wrong in `problems`: the endpoint used to answer "ok" for as
    /// long as the process was up, whatever else had stopped.
    pub fn healthz(&self) -> (bool, String) {
        let s = self.status();
        let mut problems: Vec<String> = self
            .inner
            .vitals
            .check()
            .iter()
            .map(ToString::to_string)
            .collect();
        // A node that cannot write takes no new work (ENG-4), and whatever
        // routes work to it should know.
        if let Some(why) = self.inner.engine.store_failure() {
            problems.push(format!("the op-log cannot be written: {why}"));
        }
        if let Some(hold) = self.inner.engine.restore_hold() {
            problems.push(format!(
                "this node was {} and is waiting to sync with a peer before it writes anything",
                hold.what()
            ));
        }
        if self.inner.disk_critical.load(Ordering::Relaxed) {
            problems.push("the data directory is below its free-space floor".into());
        }
        let healthy = problems.is_empty();
        let status = if !healthy {
            "unhealthy"
        } else if s.draining {
            "draining"
        } else {
            "ok"
        };
        let body = serde_json::json!({
            "status": status,
            "problems": problems,
            "version": s.version,
            "node_id": s.node_id.to_display_string(),
            "node_name": s.node_name,
            "mesh_name": s.mesh_name,
            "uptime_secs": s.uptime_secs,
            "members_alive": s.members_alive,
            "members_total": s.members_total,
            "jobs_running": s.jobs_running,
            "jobs_queued": s.jobs_queued,
            // Both numbers, because they are different questions: `slots`
            // is what this node is sized for and `slots_available` is what
            // it will start work in right now (quiet hours, `crate::quiet`).
            "slots": s.slots,
            "slots_available": s.quiet.as_ref().map_or(s.slots, |q| q.effective_slots),
        })
        .to_string()
            + "\n";
        (healthy, body)
    }

    /// `GET /metrics` body: Prometheus text exposition, gathered on scrape.
    pub fn metrics_text(&self) -> String {
        use std::fmt::Write as _;
        let s = self.status();
        let me = self.node_id();
        let (bundles_total, bundles_local, services, secrets, sources) =
            self.inner.engine.read(|st| {
                (
                    st.bundles.len(),
                    st.bundles
                        .values()
                        .filter(|b| b.holders.contains(&me))
                        .count(),
                    st.services
                        .values()
                        .filter(|e| e.current().is_some())
                        .count(),
                    st.secrets.values().filter(|e| !e.removed).count(),
                    st.sources.values().filter(|e| !e.removed).count(),
                )
            });
        // Local executor outcomes, from replicated summaries.
        let mut by_state: std::collections::BTreeMap<&'static str, usize> = Default::default();
        self.inner.engine.read(|st| {
            for j in st.jobs.values().filter(|j| j.executor == me) {
                if j.state.is_terminal() {
                    *by_state.entry(j.state.as_str()).or_default() += 1;
                }
            }
        });

        let mut out = String::with_capacity(4096);
        let mut gauge = |name: &str, help: &str, value: f64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name} {value}");
        };
        gauge(
            "meshware_uptime_seconds",
            "Seconds since this node started.",
            s.uptime_secs as f64,
        );
        gauge(
            "meshware_draining",
            "1 while the node is draining (MASTERPLAN §12).",
            if s.draining { 1.0 } else { 0.0 },
        );
        // E2.1. Worth scraping and worth alerting on: a fleet where this is
        // 0 on some nodes is a fleet where `--cpus` means two different
        // things depending on where a job lands.
        gauge(
            "meshware_cgroup_enforcing",
            "1 when declared cpu/memory limits are enforced per job (cgroups v2 delegation).",
            if self.inner.jobs.enforcing_limits() {
                1.0
            } else {
                0.0
            },
        );
        gauge(
            "meshware_members_total",
            "Machines in the mesh (not left, not revoked).",
            s.members_total as f64,
        );
        gauge(
            "meshware_members_alive",
            "Machines currently observed alive (same census as members_total).",
            s.members_alive as f64,
        );
        gauge(
            "meshware_executor_slots",
            "Concurrent executor slots on this node (MASTERPLAN §11).",
            s.slots as f64,
        );
        // The second half of the same question. Saturation computed against
        // `meshware_executor_slots` alone reads as "idle" on a node that is
        // in fact full, because quiet hours took the other twelve away
        // (`crate::quiet`). Equal to the gauge above whenever no window
        // applies, which is every node that has not configured one.
        gauge(
            "meshware_executor_slots_available",
            "Executor slots this node will start work in right now, after quiet hours.",
            s.quiet.as_ref().map_or(s.slots, |q| q.effective_slots) as f64,
        );
        gauge(
            "meshware_jobs_running",
            "Jobs executing on this node right now.",
            s.jobs_running as f64,
        );
        gauge(
            "meshware_jobs_queued",
            "Jobs waiting in this node's pending queue.",
            s.jobs_queued as f64,
        );
        gauge(
            "meshware_bundles_known",
            "Bundles known to the mesh.",
            bundles_total as f64,
        );
        gauge(
            "meshware_bundles_local",
            "Bundles held complete on this node.",
            bundles_local as f64,
        );
        gauge("meshware_services", "Live services.", services as f64);
        gauge("meshware_secrets", "Live secrets.", secrets as f64);
        gauge(
            "meshware_git_sources",
            "Registered Git sources.",
            sources as f64,
        );
        gauge(
            "meshware_clock_skew_ahead_seconds",
            "How far the freshest peer clock runs ahead of local time.",
            s.hlc_wall_ahead_micros as f64 / 1e6,
        );
        // WAN (MASTERPLAN M7): relay hub + relayed data plane.
        let relay = self.inner.transport.relay().snapshot();
        let sock = self.inner.transport.meshsock();
        gauge(
            "meshware_relay_enabled",
            "1 while this node relays for NATed members.",
            if relay.enabled { 1.0 } else { 0.0 },
        );
        gauge(
            "meshware_relay_clients",
            "NATed members registered with this relay.",
            relay.clients as f64,
        );
        gauge(
            "meshware_relay_paths_open",
            "Relayed paths currently open through this node.",
            relay.opens as f64,
        );

        // The fabric (M17 §3.8). A tunnel that grows is the failure mode a
        // forward has, so the byte counter is the point of this block —
        // "eight sessions" is reassuring right up until one of them is
        // moving a terabyte.
        gauge(
            "meshware_sessions_live",
            "Live sessions this node is party to (hosted and initiated).",
            self.inner.sessions.live_count() as f64,
        );
        gauge(
            "meshware_sessions_hosted",
            "Live sessions this node is hosting — the ones a drain closes.",
            self.inner.sessions.hosted_count() as f64,
        );
        drop(gauge);

        // -- this node's own work (ENG-21) -------------------------------
        //
        // **Counted as it happens, not counted up from state.**
        // `meshware_jobs_finished_total` used to be the number of terminal
        // job summaries this node still held — so the retention sweep made
        // it go *down*, and a counter that goes down is read by Prometheus
        // as a reset and turned into a spike of completions that never
        // happened. A counter is how many times a thing occurred; the only
        // place that is known is where it occurs.
        let meters = self.inner.jobs.meters();
        let mut counter = |name: &str, help: &str, value: u64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        };
        counter(
            "meshware_jobs_accepted_total",
            "Jobs this node took on, whether they ran at once or queued.",
            meters.accepted.load(Ordering::Relaxed),
        );
        counter(
            "meshware_jobs_started_total",
            "Jobs this node promoted out of its queue into a slot.",
            meters.started.load(Ordering::Relaxed),
        );
        counter(
            "meshware_jobs_refused_total",
            "Submissions this node refused at admission — full, out of \
             capacity, or without the device asked for. The number a node is \
             sized by.",
            meters.refused.load(Ordering::Relaxed),
        );
        drop(counter);

        // Series that only ever go up, typed as what they are (ENG-21).
        // These were `gauge`, which tells Prometheus that `rate()` is
        // meaningless on them and that a restart's drop to zero is a real
        // value rather than a reset — so a relay's throughput graph was
        // wrong in both directions.
        //
        // `meshware_members_total` and `meshware_bundles_known` keep the
        // gauge they have always had: they are counts of what exists now,
        // whatever their names suggest, and renaming a series people have
        // dashboards on is its own decision.
        let mut counter_f = |name: &str, help: &str, value: f64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        };
        counter_f(
            "meshware_relay_frames_total",
            "Frames switched by this relay.",
            relay.frames_relayed as f64,
        );
        counter_f(
            "meshware_relay_bytes_total",
            "Bytes switched by this relay.",
            relay.bytes_relayed as f64,
        );
        counter_f(
            "meshware_relay_dropped_total",
            "Frames this relay dropped (queue full / no route / oversize).",
            relay.frames_dropped as f64,
        );
        counter_f(
            "meshware_relayed_frames_sent_total",
            "Own traffic sent through relays (relay data plane).",
            sock.relayed_sent() as f64,
        );
        counter_f(
            "meshware_relayed_frames_recv_total",
            "Own traffic received through relays (relay data plane).",
            sock.relayed_recv() as f64,
        );
        counter_f(
            "meshware_sessions_opened_total",
            "Sessions admitted since this node started.",
            self.inner.sessions.opened_total.load(Ordering::Relaxed) as f64,
        );
        counter_f(
            "meshware_sessions_refused_total",
            "Sessions refused by the per-peer cap ([sessions] max_per_peer).",
            self.inner.sessions.refused_total.load(Ordering::Relaxed) as f64,
        );
        counter_f(
            "meshware_session_bytes_total",
            "Bytes moved through sessions in both directions.",
            self.inner.sessions.bytes_total.load(Ordering::Relaxed) as f64,
        );
        drop(counter_f);
        let _ = writeln!(
            out,
            "# HELP meshware_jobs_finished_total Terminal jobs executed by this node, by state, \
             counted as they finished."
        );
        let _ = writeln!(out, "# TYPE meshware_jobs_finished_total counter");
        for (state, count) in meters.finished_by_state() {
            let _ = writeln!(
                out,
                "meshware_jobs_finished_total{{state=\"{state}\"}} {count}"
            );
        }
        // What this node currently *holds*, which is a different question
        // and an honest gauge: it does go down, because a retention sweep
        // is exactly a reduction in what is held.
        let _ = writeln!(
            out,
            "# HELP meshware_jobs_recorded Terminal job records this node still holds, by state. \
             Falls as retention prunes; see meshware_jobs_finished_total for the count."
        );
        let _ = writeln!(out, "# TYPE meshware_jobs_recorded gauge");
        for (state, count) in by_state {
            let _ = writeln!(out, "meshware_jobs_recorded{{state=\"{state}\"}} {count}");
        }
        meters.queue_wait.render(
            &mut out,
            "meshware_job_queue_wait",
            "Time a job spent between arriving here and starting, in seconds.",
        );
        meters.duration.render(
            &mut out,
            "meshware_job_duration",
            "Time a job spent running on this node, in seconds.",
        );
        self.inner.ipc_latency.render(
            &mut out,
            "meshware_ipc",
            "Time from a local request arriving to this node answering it, in \
             seconds. The meter for whether the daemon is responsive, which \
             every other gauge here assumes.",
        );

        // -- the machine underneath (ENG-21) -----------------------------
        //
        // A node that is about to stop working almost always says so here
        // first: the volume filling, the process leaking handles. All three
        // are cheap enough to read on a scrape, and each is absent rather
        // than zero when it cannot be read — a disk reported as 0 bytes
        // free would page somebody at 3am for a `statvfs` that failed.
        let mut machine = |name: &str, help: &str, value: f64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name} {value}");
        };
        let data_dir = &self.inner.config.data_dir;
        if let Some(mb) = crate::capacity::disk_free_mb(data_dir) {
            machine(
                "meshware_disk_free_bytes",
                "Free space on the volume holding this node's data directory.",
                (mb * 1024 * 1024) as f64,
            );
        }
        if let Some(mb) = crate::capacity::disk_total_mb(data_dir) {
            machine(
                "meshware_disk_total_bytes",
                "Size of the volume holding this node's data directory.",
                (mb * 1024 * 1024) as f64,
            );
        }
        machine(
            "meshware_disk_critical",
            "1 when this node is below its free-space floor and refusing new \
             content (M31 A2).",
            u8::from(self.inner.disk_critical.load(Ordering::Relaxed)).into(),
        );
        machine(
            "meshware_oplog_store_bytes",
            "Op-log store size on disk, read directly rather than from the \
             census cache — so it is present on the first scrape after a restart.",
            self.inner.engine.store_bytes_on_disk() as f64,
        );
        // Named as the Prometheus process collector names them, because
        // that is what a dashboard for "is this daemon leaking" already
        // plots.
        if let Some(bytes) = process_resident_bytes() {
            machine(
                "process_resident_memory_bytes",
                "Resident set size of the meshnode process.",
                bytes as f64,
            );
        }
        if let Some(fds) = process_open_fds() {
            machine(
                "process_open_fds",
                "Open file descriptors held by the meshnode process. A node \
                 that leaks these stops accepting connections long before it \
                 stops running.",
                fds as f64,
            );
        }
        machine(
            "meshware_members_unreachable",
            "Members this node cannot currently reach — the same census \
             meshware_members_alive comes from, counted the other way.",
            s.members_total.saturating_sub(s.members_alive) as f64,
        );
        drop(machine);
        // -- dispatch latency (N6 §12, E2.5) -----------------------------
        //
        // MASTERPLAN bet #3: *"dispatch overhead is the product."* This is
        // the meter for it, and until N6 the bet was the one claim the
        // product made that nobody could check.
        self.inner.dispatch.render(
            &mut out,
            "meshware_dispatch",
            "Time from a submission arriving to a node accepting it, in seconds — \
             selector, feasibility, grants, ranking, change control, cache lookup and \
             the round trip, spills included. Stops at accepted: what follows is the \
             workload's time, not the scheduler's.",
        );

        // -- dispatch failures (ENG-6) -------------------------------------
        let failures = &self.inner.dispatch_failures;
        let _ = writeln!(
            out,
            "# HELP meshware_dispatch_failures_total Submissions an executor did not take, by why."
        );
        let _ = writeln!(out, "# TYPE meshware_dispatch_failures_total counter");
        for (reason, counter) in [
            ("unreachable", &failures.unreachable),
            ("busy", &failures.busy),
            ("undetermined", &failures.undetermined),
            ("refused", &failures.refused),
            ("too-old", &failures.too_old),
        ] {
            let _ = writeln!(
                out,
                "meshware_dispatch_failures_total{{reason=\"{reason}\"}} {}",
                counter.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP meshware_dispatch_spills_total Submissions that moved on to another candidate."
        );
        let _ = writeln!(out, "# TYPE meshware_dispatch_spills_total counter");
        let _ = writeln!(
            out,
            "meshware_dispatch_spills_total {}",
            failures.spilled.load(Ordering::Relaxed)
        );

        // -- group work (ENG-21) -----------------------------------------
        //
        // The three ways one submission becomes many jobs, counted on the
        // node that *owns* the run. `meshware_jobs_*` is the other half:
        // it counts what this node executes, and the owner of a 200-target
        // fan-out executes none of it.
        self.inner.groups.render(&mut out);

        // -- what the mesh looks like from here (ENG-21) ------------------
        //
        // Per peer, and labelled by node id rather than name: a name is
        // editable and two members may share one, and a series that
        // silently merges two machines is worse than no series. The name
        // rides along as a second label for the dashboard to show.
        //
        // Bounded by the mesh: one sample per member per series, which at
        // the 200-machine ceiling the Fleet table is built for is 600
        // samples — a scrape, not a cardinality problem.
        // The same census `meshware_members_total` is built from — not
        // left, not revoked — so the per-peer rows and the fleet-wide
        // counts describe one mesh.
        let peers: Vec<(NodeId, String)> = self.inner.engine.read(|st| {
            st.members
                .values()
                .filter(|m| !m.left && !st.revoked.contains_key(&m.info.node_id))
                .filter(|m| m.info.node_id != me)
                .map(|m| (m.info.node_id, m.info.name.clone()))
                .collect()
        });
        let _ = writeln!(
            out,
            "# HELP meshware_peer_alive 1 while this node currently observes a peer as alive. One sample per member of the mesh, from this node's point of view: a peer that is up everywhere else and 0 here is a partition, and no fleet-wide gauge can show that."
        );
        let _ = writeln!(out, "# TYPE meshware_peer_alive gauge");
        for (id, name) in &peers {
            let _ = writeln!(
                out,
                "meshware_peer_alive{{peer=\"{}\",name=\"{}\"}} {}",
                id.to_display_string(),
                escape_label(name),
                u8::from(matches!(self.inner.gossip.liveness_of(id), Liveness::Alive))
            );
        }
        // Sync lag: how far a peer trailed this node's log the last time it
        // asked for ops. Absent rather than zero for a peer that has not
        // asked — "we do not know" and "it is caught up" are different
        // states, and a 0 invented for the first would hide a peer that
        // stopped syncing altogether.
        let lags: Vec<(&NodeId, &String, crate::gossip::SyncLag)> = peers
            .iter()
            .filter_map(|(id, name)| self.inner.gossip.sync_lag(id).map(|l| (id, name, l)))
            .collect();
        if !lags.is_empty() {
            let _ = writeln!(
                out,
                "# HELP meshware_peer_sync_behind_ops Ops of this node's log a peer had not \
                 received when it last asked (ENG-21). Absent for a peer that has not asked."
            );
            let _ = writeln!(out, "# TYPE meshware_peer_sync_behind_ops gauge");
            for (id, name, lag) in &lags {
                let _ = writeln!(
                    out,
                    "meshware_peer_sync_behind_ops{{peer=\"{}\",name=\"{}\"}} {}",
                    id.to_display_string(),
                    escape_label(name),
                    lag.behind
                );
            }
            let _ = writeln!(
                out,
                "# HELP meshware_peer_sync_age_seconds How long ago a peer last asked this node \
                 for ops. Rising on a peer that is alive is the shape of a stuck sync."
            );
            let _ = writeln!(out, "# TYPE meshware_peer_sync_age_seconds gauge");
            for (id, name, lag) in &lags {
                let _ = writeln!(
                    out,
                    "meshware_peer_sync_age_seconds{{peer=\"{}\",name=\"{}\"}} {}",
                    id.to_display_string(),
                    escape_label(name),
                    lag.observed.elapsed().as_secs_f64()
                );
            }
        }

        // -- ops that cannot be applied yet (ENG-21) ----------------------
        let _ = writeln!(
            out,
            "# HELP meshware_ops_parked Ops the last apply pass could not take, because whatever \
             would authorize them has not arrived. A gauge: a parked op is re-fetched on the next \
             sync rather than consumed, so what matters is the standing backlog. Zero on a \
             healthy node."
        );
        let _ = writeln!(out, "# TYPE meshware_ops_parked gauge");
        let _ = writeln!(
            out,
            "meshware_ops_parked {}",
            self.inner.engine.parked_ops()
        );

        // -- what was turned away (SEC-19, M31 A3, ENG-21) ----------------
        //
        // Every limiter on the node, in one family. A limiter nobody can
        // graph is a limiter whose first sign of biting is a support
        // ticket, and three of these sit in front of doors that are open
        // to anybody holding a key.
        let anon = &self.inner.anon;
        let guard = self.inner.engine.rate_guard();
        let _ = writeln!(
            out,
            "# HELP meshware_refusals_total Requests and ops a limit turned away, by which limit. \
             `op_rate` is the per-entity op-rate guard; `enrol_*` and `login_*` are the two doors \
             open to any key holder, limited per source and overall."
        );
        let _ = writeln!(out, "# TYPE meshware_refusals_total counter");
        for (limit, value) in [
            ("op_rate", guard.refused_total()),
            ("enrol_source", anon.enrol_source.refused()),
            ("enrol_all", anon.enrol_all.refused()),
            ("login_source", anon.login_source.refused()),
            ("login_all", anon.login_all.refused()),
        ] {
            let _ = writeln!(out, "meshware_refusals_total{{limit=\"{limit}\"}} {value}");
        }
        let _ = writeln!(
            out,
            "# HELP meshware_op_rate_pauses_total Times the op-rate guard put an entity in a \
             pause after repeated refusals (M31 A3). Each one is a workload that kept pushing \
             after being told to stop."
        );
        let _ = writeln!(out, "# TYPE meshware_op_rate_pauses_total counter");
        let _ = writeln!(
            out,
            "meshware_op_rate_pauses_total {}",
            guard.paused_total()
        );

        // -- accelerators (N6 §12, C5) -----------------------------------
        //
        // **This node's own cards.** An exporter reports the machine it
        // runs on; a node exporting its peers' GPUs would double-count
        // every card in the mesh once per node that can see it.
        //
        // Absent on a machine with no accelerators and absent on one whose
        // telemetry has not arrived, which are different states and both
        // honest: a gauge invented as `0` for a card nobody can read would
        // show a saturated GPU as idle.
        render_devices(
            &mut out,
            &self.inner.jobs.devices(),
            &self.inner.jobs.device_pressure(),
            &self.inner.jobs.device_holders(),
        );

        // -- the op-log, by entity (M31 P0) ------------------------------
        //
        // From the cache only: a scrape must never be the thing that walks
        // the log. Absent on the first scrape after a restart, and filled
        // by the compaction tick within the hour.
        if let Some(census) = self.cached_census() {
            let _ = writeln!(
                out,
                "# HELP meshware_oplog_bytes Op-log size on disk (M31 P0).\n\
                 # TYPE meshware_oplog_bytes gauge\n\
                 meshware_oplog_bytes {}",
                census.disk_bytes
            );
            let _ = writeln!(
                out,
                "# HELP meshware_oplog_surviving_bytes_per_day Estimated bytes of permanent \
                 op-log growth per day — what a year of this mesh costs.\n\
                 # TYPE meshware_oplog_surviving_bytes_per_day gauge\n\
                 meshware_oplog_surviving_bytes_per_day {}",
                census.surviving_bytes_per_day()
            );
            let _ = writeln!(
                out,
                "# HELP meshware_oplog_ops_total Ops in the log, by entity kind."
            );
            let _ = writeln!(out, "# TYPE meshware_oplog_ops_total gauge");
            for row in &census.by_kind {
                let _ = writeln!(
                    out,
                    "meshware_oplog_ops_total{{kind=\"{}\"}} {}",
                    row.kind, row.ops
                );
            }
            // The second half of the same question, and the one that
            // predicts next year: how many of those a compaction pass would
            // never be able to drop.
            let _ = writeln!(
                out,
                "# HELP meshware_oplog_surviving_ops Ops no compaction pass can ever drop, by entity kind."
            );
            let _ = writeln!(out, "# TYPE meshware_oplog_surviving_ops gauge");
            for row in &census.by_kind {
                let _ = writeln!(
                    out,
                    "meshware_oplog_surviving_ops{{kind=\"{}\"}} {}",
                    row.kind, row.surviving
                );
            }
        }

        // -- sinks (M31 B1 item 10) --------------------------------------
        //
        // Four series, and the one that matters is `_dropped_total`: lag is
        // a sink catching up and dropping is history that is gone. A
        // dashboard built on these should alert on the second and graph the
        // first.
        let sinks = self.sink_rows();
        if !sinks.is_empty() {
            for (name, help, kind) in [
                (
                    "meshware_sink_lag_seconds",
                    "How far behind a sink is, in the data's own clock.",
                    "gauge",
                ),
                (
                    "meshware_sink_sent_total",
                    "Rows a sink has accepted.",
                    "counter",
                ),
                (
                    "meshware_sink_spooled_bytes",
                    "Bytes a sink is holding because its destination refused them.",
                    "gauge",
                ),
                (
                    "meshware_sink_dropped_total",
                    "Batches dropped because the spool filled. These rows are gone.",
                    "counter",
                ),
            ] {
                let _ = writeln!(out, "# HELP {name} {help}");
                let _ = writeln!(out, "# TYPE {name} {kind}");
                for row in &sinks {
                    let value = match name {
                        "meshware_sink_lag_seconds" => row.lag_secs,
                        "meshware_sink_sent_total" => row.sent,
                        "meshware_sink_spooled_bytes" => row.spooled_bytes,
                        _ => row.dropped,
                    };
                    // A sink's name is whatever an operator typed; one
                    // quote in it made the whole exposition unparseable
                    // (SEC-23).
                    let _ = writeln!(
                        out,
                        "{name}{{sink=\"{}\"}} {value}",
                        escape_label(&row.name)
                    );
                }
            }
        }

        let _ = writeln!(
            out,
            "# HELP meshware_build_info Build metadata.\n# TYPE meshware_build_info gauge\nmeshware_build_info{{version=\"{}\",protocol=\"{}.{}\"}} 1",
            s.version, s.proto_major, s.proto_minor
        );
        out
    }
}
