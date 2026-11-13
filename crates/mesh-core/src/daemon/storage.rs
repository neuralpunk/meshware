//! What this node is holding, and reclaiming it: retention windows, the
//! content store, disk pressure and op-log compaction.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// C0 §4.3 — the op-log figures behind `meshnode state show`.
    pub(super) fn state_stats(&self) -> crate::error::Result<mesh_proto::StateStats> {
        let horizon = self.inner.config.state.horizon_secs();
        let s = self.inner.engine.op_stats(horizon)?;
        Ok(mesh_proto::StateStats {
            ops_total: s.ops_total,
            ops_mine: s.ops_mine,
            pruned_mine: s.pruned_mine,
            pruned_mesh: s.pruned_mesh,
            droppable_now: s.droppable_now,
            held_by_horizon: s.held_by_horizon,
            horizon_secs: s.horizon_secs,
            usage_live: s.usage_live,
            usage_expired: s.usage_expired,
            bytes_on_disk: s.bytes_on_disk,
            dropped: 0,
            checkpoint_seq: 0,
            dry_run: false,
        })
    }

    /// The retention policy, plus what it does to the size of the log
    /// (M31 A1 item 5).
    ///
    /// **The projection is not optional, and it is the point of the
    /// screen.** A knob whose effect nobody can predict is a knob nobody
    /// turns; the number that makes it a decision is *"with these settings
    /// the log settles at ~1.6 GB"*, shown before the policy is signed.
    ///
    /// It splits growth in two, because the halves behave differently and
    /// conflating them would make retention look more powerful than it is:
    ///
    /// - **retirable** — closed job records, which the `runs` window
    ///   actually drops. Multiply by the window and that is the steady
    ///   state.
    /// - **permanent** — everything a compaction pass can never drop at any
    ///   horizon, the audit log above all. This grows for ever whatever the
    ///   operator types, and a projection that hid it would be a lie the
    ///   first time somebody asked why the log kept growing after they set
    ///   `runs: 7d`.
    pub(super) fn retention_view(&self) -> mesh_proto::RetentionView {
        use mesh_proto::{RetentionTable, retention::describe_days};

        let (policy, hlc) = self
            .inner
            .engine
            .read(|s| (s.retention.policy.clone(), s.retention.updated_hlc));
        let local = *self.inner.retention.lock();
        let node_history_days = self.inner.config.history.keep_days;

        let windows = RetentionTable::ALL
            .into_iter()
            .map(|table| {
                let w = policy.window(table);
                // What this node actually applies. Only two tables have a
                // node-local preference to fold in; the rest resolve to the
                // mesh window unchanged.
                let node_pref = match table {
                    RetentionTable::JobOutput => (local.job_days > 0).then_some(local.job_days),
                    RetentionTable::Runs => (node_history_days > 0).then_some(node_history_days),
                    _ => None,
                };
                mesh_proto::RetentionRow {
                    table: table.as_str().to_string(),
                    window: describe_days(w.days),
                    floor: (w.floor_days > 0).then(|| describe_days(w.floor_days)),
                    effective_here: describe_days(w.resolve_for_node(node_pref)),
                    // Only a finite window bounds anything: `audit` under
                    // `forever` keeps every row (ENG-3).
                    bounds_the_op_log: table.bounds_the_op_log() && !w.is_forever(),
                }
            })
            .collect();

        // From the cache: this is a screen, and a screen must not be the
        // thing that walks the op-log. Absent right after a restart, which
        // costs the projection and nothing else.
        let census = self.cached_census();
        let runs = policy.window(RetentionTable::Runs);
        let (oplog_bytes, permanent, retirable) = match &census {
            None => (0, 0, 0),
            Some(c) => {
                let permanent = c.surviving_bytes_per_day();
                // What the `runs` window retires: ops written in the last
                // day that a pass will eventually drop, closed records
                // among them. Derived by difference rather than counted
                // separately, so the two halves always sum to the whole.
                let mean = c.bytes_last_day.checked_div(c.ops_last_day).unwrap_or(0);
                let retirable_ops = c.ops_last_day.saturating_sub(c.surviving_last_day);
                (c.disk_bytes, permanent, mean.saturating_mul(retirable_ops))
            }
        };

        mesh_proto::RetentionView {
            policy: policy.clone(),
            windows,
            signed_by: policy.is_signed().then(|| {
                self.inner.engine.read(|s| {
                    crate::rbac::username_of(
                        s,
                        &self.inner.engine.genesis().body.founder_user,
                        &policy.created_by,
                    )
                })
            }),
            signed_at_micros: if policy.is_signed() {
                policy.created_at_micros.max(hlc.wall_micros)
            } else {
                0
            },
            oplog_bytes,
            permanent_bytes_per_day: permanent,
            retirable_bytes_per_day: retirable,
            // A finite window settles at (retirable × days) plus whatever
            // is permanent and never settles at all — so the honest answer
            // to "where does this end up in a year" is the sum, and it is
            // `None` when `runs` is forever because then it does not end up
            // anywhere.
            settles_at_bytes: (!runs.is_forever()).then(|| {
                retirable
                    .saturating_mul(u64::from(runs.days))
                    .saturating_add(permanent.saturating_mul(365))
            }),
        }
    }

    /// This node's job-output retention, after the mesh policy is applied
    /// (M31 A1, Q8 decided).
    ///
    /// **The mesh policy is the default and a node may keep less, but not
    /// more.** The machine with the small disk is a real machine, and a
    /// mesh-wide policy that fills it is a support ticket; so `[retention]
    /// job_days` shorter than the mesh window wins, and longer is clamped
    /// down. A floor, when one is set, clamps the other way and wins over
    /// both — that is what an auditor is buying.
    ///
    /// The **size** cap stays purely node-local and is not touched here:
    /// "this disk holds a gigabyte of logs" is a fact about a machine, not
    /// a policy about history, and there is nothing for the mesh to say
    /// about it.
    pub(super) fn resolved_retention(&self) -> crate::config::Retention {
        let local = *self.inner.retention.lock();
        let policy = self.inner.engine.retention();
        let window = policy.window(mesh_proto::RetentionTable::JobOutput);
        // The content window resolves the same way and for the same reason:
        // the mesh policy is the ceiling, this node may keep less. A node
        // asking `keep` where the mesh says thirty days gets thirty days —
        // `resolve_for_node` reads a request of `None` as the default and
        // `FOREVER` as asking for more, which is the case it refuses.
        crate::config::Retention {
            job_days: window.resolve_for_node((local.job_days > 0).then_some(local.job_days)),
            job_max_bytes: local.job_max_bytes,
            ..local
        }
    }

    /// The content window this node actually applies: `(days, immediately)`.
    ///
    /// **`Ephemeral` bypasses the day arithmetic rather than passing
    /// through it**, and that is not a shortcut — it is the only correct
    /// answer. `FOREVER` is zero days by this module's long-standing
    /// convention, and "reclaim with no grace period" is also zero days, so
    /// handing `Some(0)` to [`RetentionWindow::resolve_for_node`] reads as
    /// *asking to keep more than the mesh allows* and gets clamped up to
    /// the mesh default — turning the most aggressive setting into one of
    /// the most conservative. It did exactly that: `ephemeral` under a 7d
    /// mesh policy resolved to 7d.
    ///
    /// The reason no ceiling is needed here is the rule itself. The mesh
    /// policy caps how much a node may *keep*, and `ephemeral` keeps less
    /// than any window can express, so there is nothing for a ceiling to
    /// object to.
    pub(super) fn resolved_content_window(&self) -> (u32, bool) {
        let local = *self.inner.retention.lock();
        if local.content.reclaims_immediately() && local.content_days.is_none() {
            return (0, true);
        }
        let window = self
            .inner
            .engine
            .retention()
            .window(mesh_proto::RetentionTable::Content);
        (window.resolve_for_node(local.content_request()), false)
    }

    /// What this node's content store holds (M31 A2).
    pub(super) fn content_status(&self) -> mesh_proto::ContentStatusView {
        let plan = self.content_sweep_plan(true);
        let usage = self.inner.content.usage(&plan);
        let local = *self.inner.retention.lock();
        let dir = &self.inner.config.data_dir;
        let disk = self.live_config().disk;
        let mb = |v: Option<u64>| v.unwrap_or(0).saturating_mul(1024 * 1024);
        let (window_days, immediate) = self.resolved_content_window();
        mesh_proto::ContentStatusView {
            policy: local.content.to_string(),
            window_days,
            // The node asked for one number and resolved to a smaller one:
            // the mesh policy capped it. Worth saying out loud, because
            // otherwise the settings screen and the config file disagree
            // and neither explains why.
            capped_by_mesh: !immediate
                && match local.content_request() {
                    // `keep`, and an explicit `content_days = 0`, both mean
                    // unbounded — and forever is *zero*, so a plain
                    // `want > window` comparison reads them as the shortest
                    // window rather than the longest. A finite mesh window
                    // is the mesh overruling either of them.
                    None | Some(0) => window_days > 0,
                    Some(want) => window_days > 0 && window_days < want,
                },
            bundles: usage.bundles as u64,
            chunks: usage.chunks as u64,
            bytes: usage.bytes,
            protected: plan.protected.len() as u64,
            reclaimable_bundles: usage.reclaimable_bundles as u64,
            reclaimable_bytes: usage.reclaimable_bytes,
            max_bytes: local.content_max_bytes,
            disk_free_bytes: mb(crate::capacity::disk_free_mb(dir)),
            disk_total_bytes: mb(crate::capacity::disk_total_mb(dir)),
            disk_floor_bytes: disk.min_free_bytes(dir),
            disk_warn_bytes: disk.warn_bytes(dir),
            disk_critical: self.disk_critical(),
        }
    }

    /// Reclaim on demand (M31 A2).
    ///
    /// `aggressive` drops the window entirely and takes everything nothing
    /// still needs. It is the same code path with `reclaim_immediately`
    /// set, not a second implementation — the protected set is computed
    /// identically, so the most aggressive thing an operator can ask for
    /// still cannot touch a running deployment's code.
    pub(super) fn content_prune(
        &self,
        dry_run: bool,
        aggressive: bool,
    ) -> mesh_proto::ContentPruneView {
        let mut plan = self.content_sweep_plan(dry_run);
        if aggressive {
            plan.reclaim_immediately = true;
        }
        let report = self.inner.content.sweep(&plan).unwrap_or_default();
        let (orphan_chunks, orphan_bytes) = self
            .inner
            .content
            .collect_orphans(dry_run)
            .unwrap_or((0, 0));
        if !dry_run && !report.bundles_removed.is_empty() {
            info!(
                bundles = report.bundles_removed.len(),
                mb = report.bytes_reclaimed / (1024 * 1024),
                aggressive,
                "content reclaim requested"
            );
        }
        mesh_proto::ContentPruneView {
            dry_run,
            bundles_removed: report.bundles_removed.len() as u64,
            chunks_removed: report.chunks_removed as u64,
            bytes_reclaimed: report.bytes_reclaimed,
            bytes_remaining: report.bytes_remaining,
            protected: report.protected as u64,
            orphan_chunks: orphan_chunks as u64,
            orphan_bytes,
        }
    }

    /// Free space across the mesh, from gossip (M31 A2).
    ///
    /// Read off the pressure snapshots every node already publishes, so
    /// this costs one state read and no new wire traffic. A node whose
    /// snapshot has not arrived reports `unknown` rather than zero — the
    /// difference between *no room* and *no answer* is the whole value of
    /// the screen.
    pub(super) fn disk_rows(&self) -> Vec<mesh_proto::DiskRow> {
        let floor = self
            .inner
            .config
            .disk
            .min_free_bytes(&self.inner.config.data_dir);
        let warn = self
            .inner
            .config
            .disk
            .warn_bytes(&self.inner.config.data_dir);
        let me = self.node_id();
        let members: Vec<(NodeId, String, Vec<String>, bool)> = self.inner.engine.read(|s| {
            s.members
                .iter()
                .map(|(id, m)| {
                    (
                        *id,
                        m.info.name.clone(),
                        m.info.tags.clone(),
                        s.is_active_member(id),
                    )
                })
                .collect()
        });
        let mut rows = {
            members
                .into_iter()
                .map(|(id, name, tags, online)| {
                    let total = mesh_proto::Capacity::from_tags(&tags)
                        .map(|c| c.disk_mb)
                        .or_else(|| {
                            (id == me)
                                .then(|| {
                                    crate::capacity::disk_total_mb(&self.inner.config.data_dir)
                                })
                                .flatten()
                        })
                        .unwrap_or(0)
                        .saturating_mul(1024 * 1024);
                    // This node measures itself. Gossip is how the *other*
                    // rows get here, and a node does not gossip to itself —
                    // so reading everyone the same way left the one row an
                    // operator is most likely to be looking at as
                    // `unknown`, on the screen they opened because they
                    // were worried about this machine.
                    let free = if id == me {
                        crate::capacity::disk_free_mb(&self.inner.config.data_dir)
                    } else {
                        self.inner
                            .gossip
                            .pressure_of(&id)
                            .and_then(|p| p.disk_free_mb)
                    }
                    .map(|mb| mb.saturating_mul(1024 * 1024));
                    // The thresholds are this node's, applied to everyone.
                    // Not strictly right — the floor is node-local and a
                    // peer may have set its own — but a fleet-wide screen
                    // needs one yardstick, and "which of my nodes would I
                    // be worried about" is the question being asked.
                    let state = match free {
                        None => "unknown",
                        Some(f) if f < floor => "low",
                        Some(f) if f < warn => "warn",
                        Some(_) => "ok",
                    };
                    mesh_proto::DiskRow {
                        node_id: id,
                        name,
                        online,
                        free_bytes: free.unwrap_or(0),
                        total_bytes: total,
                        state: state.to_string(),
                    }
                })
                .collect::<Vec<_>>()
        };
        // Worst first: a fleet screen exists to surface the node in
        // trouble, and sorting by name buries it on page three.
        rows.sort_by(|a, b| {
            let rank = |r: &mesh_proto::DiskRow| match r.state.as_str() {
                "low" => 0,
                "warn" => 1,
                "ok" => 2,
                _ => 3,
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.free_bytes.cmp(&b.free_bytes))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// Reclaim the content store, and say so if it took anything.
    ///
    /// `why` names the trigger — `boot`, `retention`, `disk` — because the
    /// same line arriving for three different reasons is three different
    /// facts, and the one an operator needs to see at 3am is the third.
    /// The housekeeping sweeps, off the async workers (ENG-26).
    ///
    /// All three walk the filesystem: the content store is every release,
    /// every source sync and every bundle ever deployed here, and the
    /// retention sweep is a directory per job. They ran on the scheduler
    /// task, so on a node with a large store the tick that reclaims disk
    /// also stopped that runtime thread serving anything for the length of
    /// the walk — and the bigger the store, the longer the stall, which is
    /// the wrong way round for a mechanism whose job is to keep a node
    /// healthy.
    pub(super) async fn sweep_off_thread(&self, what: Sweep, why: &'static str) {
        let this = self.clone();
        let retention = matches!(what, Sweep::Retention).then(|| self.resolved_retention());
        let _ = tokio::task::spawn_blocking(move || match what {
            Sweep::Content => this.sweep_content(why),
            Sweep::Objects => this.sweep_objects(),
            Sweep::Retention => {
                if let Some(policy) = retention {
                    this.inner.jobs.sweep_retention(policy);
                }
            }
        })
        .await;
    }

    pub(super) fn sweep_content(&self, why: &str) {
        let plan = self.content_sweep_plan(false);
        if plan.protected.is_empty() && self.inner.engine.read(|s| s.bundles.is_empty()) {
            return;
        }
        match self.inner.content.sweep(&plan) {
            Ok(report) if !report.bundles_removed.is_empty() => info!(
                trigger = why,
                bundles = report.bundles_removed.len(),
                chunks = report.chunks_removed,
                mb = report.bytes_reclaimed / (1024 * 1024),
                protected = report.protected,
                "reclaimed content store"
            ),
            Ok(_) => {}
            Err(e) => warn!("content reclaim failed: {e}"),
        }
        // Orphans are cheap to look for and free to leave, so this rides
        // along with the sweep rather than earning a timer of its own.
        match self.inner.content.collect_orphans(false) {
            Ok((n, bytes)) if n > 0 => info!(
                chunks = n,
                mb = bytes / (1024 * 1024),
                "collected orphaned chunks"
            ),
            _ => {}
        }
    }

    /// Is this node running out of room, and if so, do something about it.
    ///
    /// The order matters and is the whole design: **warn, then reclaim,
    /// then refuse**. Warning first means an operator hears about it while
    /// they still have choices. Reclaiming second means the product cleans
    /// up after itself before it asks anyone for help. Refusing last means
    /// that when meshware genuinely cannot help — the disk is full of
    /// somebody else's data — it stops making the problem worse instead of
    /// being the process that finally fills the volume.
    ///
    /// `warned` carries the last verdict so a node parked below the line
    /// says so once and on recovery, not sixty times an hour.
    pub(super) fn check_disk_pressure(&self, warned: &mut bool) {
        let disk = self.live_config().disk;
        let dir = &self.inner.config.data_dir;
        // Unmeasurable is not the same as full — but zero is (ENG-4): a
        // volume with nothing left read as "unmeasurable" and was never
        // called critical.
        let Some(free) =
            crate::capacity::disk_free_mb(dir).map(|mb| mb.saturating_mul(1024 * 1024))
        else {
            return;
        };
        let floor = disk.min_free_bytes(dir);
        let warn_at = disk.warn_bytes(dir);
        let gb = |b: u64| format!("{:.1} GB", b as f64 / 1e9);

        if free >= warn_at {
            if *warned {
                *warned = false;
                info!(free = %gb(free), "disk pressure cleared");
            }
            self.inner.disk_critical.store(false, Ordering::Relaxed);
            self.inner.jobs.set_output_paused(false);
            return;
        }
        if !*warned {
            *warned = true;
            warn!(
                free = %gb(free),
                floor = %gb(floor),
                "low disk at the data dir — meshware will reclaim its content store to stay above the floor"
            );
        }
        if free >= floor {
            return; // in the warning band, not yet at the floor
        }
        if disk.reclaim_under_pressure {
            self.sweep_content("disk");
        }
        // Re-measure: the sweep may well have fixed it.
        let after = crate::capacity::disk_free_mb(dir)
            .map(|mb| mb.saturating_mul(1024 * 1024))
            .unwrap_or(free);
        let critical = after < floor;
        // Job output is what fills a disk fastest; while under the floor it
        // is not kept, so the op-log keeps the room it needs (ENG-4).
        self.inner.jobs.set_output_paused(critical);
        if critical != self.inner.disk_critical.swap(critical, Ordering::Relaxed) {
            if critical {
                warn!(
                    free = %gb(after),
                    floor = %gb(floor),
                    "still under the disk floor after reclaiming — refusing new content until there is room"
                );
            } else {
                info!(free = %gb(after), "back above the disk floor");
            }
        }
    }

    /// Change this node's content-store posture while it is running.
    ///
    /// A setter rather than a config reload, because the caller is Director
    /// — which has no `meshnode.toml` to reload and would otherwise have to
    /// restart its own engine to change a tidiness setting. The next sweep
    /// picks it up; nothing here deletes anything, which is why a settings
    /// screen can call it on every keystroke without consequence.
    pub fn set_content_retention(
        &self,
        policy: mesh_proto::ContentPolicy,
        max_bytes: u64,
    ) -> crate::config::Retention {
        let mut retention = self.inner.retention.lock();
        retention.content = policy;
        retention.content_max_bytes = max_bytes;
        *retention
    }

    /// Is this node refusing new content because it is out of room?
    pub fn disk_critical(&self) -> bool {
        self.inner.disk_critical.load(Ordering::Relaxed)
    }

    /// The sweep this node's settings and the mesh policy add up to.
    ///
    /// Assembled here rather than in `ContentStore` because every input
    /// lives out here: the resolved window, replicated state's opinion of
    /// what is still needed, and this node's own version — which is what
    /// keeps its own release local when everything older goes.
    pub(super) fn content_sweep_plan(&self, dry_run: bool) -> crate::content::SweepPlan {
        let retention = self.resolved_retention();
        let (days, immediate) = self.resolved_content_window();
        let disk = self.live_config().disk;
        crate::content::SweepPlan {
            protected: self
                .inner
                .engine
                .read(|s| s.referenced_bundles(Some(VERSION))),
            cutoff_micros: (days > 0).then(|| {
                mesh_proto::now_micros().saturating_sub(u64::from(days) * 86_400 * 1_000_000)
            }),
            reclaim_immediately: immediate,
            max_bytes: retention.content_max_bytes,
            min_free_bytes: disk.min_free_bytes(&self.inner.config.data_dir),
            dry_run,
        }
    }

    /// The op-log census, recomputed if the cached one has gone stale.
    ///
    /// One scan of the log, at most [`CENSUS_TTL`] apart. Every surface that
    /// reports what the log is made of goes through here, so `doctor`,
    /// `/metrics` and `state show` cannot disagree with each other about the
    /// same node at the same moment.
    pub fn op_census(&self) -> crate::error::Result<mesh_proto::OpCensus> {
        {
            let cached = self.inner.census.lock();
            if let Some((taken, census)) = cached.as_ref()
                && taken.elapsed() < CENSUS_TTL
            {
                return Ok(census.clone());
            }
        }
        let census = self.inner.engine.census()?;
        *self.inner.census.lock() = Some((Instant::now(), census.clone()));
        Ok(census)
    }

    /// The census as `/metrics` wants it: whatever is cached, and nothing
    /// if there is none yet.
    ///
    /// A scrape must never be the thing that walks the log — a Prometheus
    /// server with a two-second timeout pointed at a node with a million
    /// ops would turn a measurement into an outage. The first scrape after
    /// a restart is therefore short two gauges, and the compaction tick
    /// fills the cache within the hour regardless of who asks.
    pub(super) fn cached_census(&self) -> Option<mesh_proto::OpCensus> {
        self.inner.census.lock().as_ref().map(|(_, c)| c.clone())
    }

    /// Run one compaction pass over this node's own authored log, and
    /// report what it cost — "ops before and after", which is C0 P1's demo.
    pub(super) fn state_compact(
        &self,
        horizon_secs: Option<u64>,
        dry_run: bool,
    ) -> crate::error::Result<mesh_proto::StateStats> {
        let horizon = horizon_secs.unwrap_or_else(|| self.inner.config.state.horizon_secs());
        if dry_run {
            let plan = self.inner.engine.compaction_plan(horizon)?;
            let mut stats = self.state_stats()?;
            stats.horizon_secs = horizon;
            // `drop` is what this pass would newly take (M-11); subtracting
            // what earlier passes took as well under-reported it.
            stats.dropped = plan.drop.len() as u64;
            stats.held_by_horizon = plan.held_by_horizon;
            stats.dry_run = true;
            return Ok(stats);
        }
        let (dropped, checkpoint_seq) = self.inner.engine.compact_self(horizon)?;
        let mut stats = self.state_stats()?;
        stats.horizon_secs = horizon;
        stats.dropped = dropped;
        stats.checkpoint_seq = checkpoint_seq;
        Ok(stats)
    }
}
