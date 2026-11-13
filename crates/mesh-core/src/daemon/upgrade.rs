//! Fleet upgrade: staging a signed release, the wave gate, and the rows
//! `meshnode upgrade --status` reads.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// Serve `Request::UpgradeSelf`: resolve the plan against *this node's
    /// own state*, then hand the swap to a process that outlives this
    /// daemon.
    ///
    /// Everything authorizing this is re-derived here rather than carried
    /// in: the plan's admin signature was verified at merge time, and what
    /// this checks now is that it still means something — the signer is
    /// still an active admin, the plan has not expired or been withdrawn,
    /// and its selector names this machine. A compromised driver is a
    /// courier, and revoking the admin stops the rollout everywhere it has
    /// not reached, with no signature to recall (the M8 property, for the
    /// same reason).
    pub(super) fn serve_upgrade_self(&self, plan_id: mesh_proto::UpgradeId) -> Response {
        let founder = self.inner.engine.genesis().body.founder_user;
        let me = self.node_id();
        let now = mesh_proto::now_micros();
        // Each refusal says whether asking again could ever help. Only one
        // of them can: a plan that has not gossiped here *yet*.
        let resolved = self.inner.engine.read(|s| {
            let Some(entry) = s.upgrade_plans.get(&plan_id) else {
                return Err((
                    format!(
                        "no upgrade plan {plan_id} here — it may not have reached this node yet"
                    ),
                    true,
                ));
            };
            if entry.cancelled.is_some() {
                return Err(("that upgrade plan was withdrawn".to_string(), false));
            }
            if entry.plan.is_expired_at(now) {
                return Err(("that upgrade plan has expired".to_string(), false));
            }
            // Asked here, against this node's own state. The plan carries
            // an admin's signature; whether that admin *is still* an admin
            // is a question only the acting node can answer honestly.
            if !rbac::has_role(s, &founder, &entry.plan.created_by, rbac::ADMIN_ONLY, now) {
                return Err((
                    "the admin who signed that plan may no longer upgrade nodes".to_string(),
                    false,
                ));
            }
            let Some(member) = s.members.get(&me) else {
                return Err((
                    "this node is not a member of its own state".to_string(),
                    true,
                ));
            };
            if !Selector::parse(&entry.plan.selector).matches_in(s, &member.info) {
                return Err((
                    format!(
                        "this node is not named by {:?} — refusing an upgrade nobody asked this \
                         machine for",
                        entry.plan.selector
                    ),
                    false,
                ));
            }
            let target = normalize_release_tag(&entry.plan.version);
            // The record for *this machine's* architecture. A version
            // published for another architecture only is a terminal
            // refusal, not a wait: gossip will never bring a binary that
            // was never built.
            let release =
                crate::release::for_arch(&s.releases, &target, std::env::consts::ARCH).cloned();
            if release.is_none() {
                let published = crate::release::arches_for(&s.releases, &target);
                if !published.is_empty() {
                    return Err((
                        format!(
                            "release {target} is published for {} only, and this node is {} — \
                             publish a {} build of {target} (Director → Updates, or `meshnode \
                             upgrade --publish`) and run the rollout again",
                            published.join(", "),
                            std::env::consts::ARCH,
                            std::env::consts::ARCH
                        ),
                        false,
                    ));
                }
            }
            Ok((
                entry.plan.created_by,
                target,
                entry.plan.move_pending,
                release,
                entry.force,
                entry.plan.created_at_micros,
            ))
        });
        let resolved = resolved.and_then(|v| {
            // This node already tried the target since the plan was made,
            // and it did not start (ENG-12). Asking again would install it
            // again, fail again, and roll back again — the rollout stops
            // here instead, and says why. A plan made *after* the rollback
            // is somebody deciding to try again, and is honoured.
            let target = v.1.trim_start_matches('v');
            match crate::upgrade_guard::rolled_back(&self.inner.config.data_dir) {
                Some(r) if r.failed == target && r.at_micros >= v.5 && VERSION != target => Err((
                    format!(
                        "meshnode {target} started {} times here without becoming healthy \
                             and was rolled back to {} — see `journalctl -u meshnode`; the \
                             binary that failed is kept at {}",
                        r.starts,
                        r.restored,
                        r.failed_exe.display()
                    ),
                    false,
                )),
                _ => Ok(v),
            }
        });
        let (by, target, move_pending, release, force, _) = match resolved {
            Ok(v) => v,
            // The refusal travels back to the driver, which is where a
            // person is watching. It is not audited here: an op-log entry
            // per poll of a node that is simply waiting for gossip would
            // bury the entries that matter under the ones that do not.
            Err((message, retry)) => {
                // Not audited — an op-log entry per poll of a node waiting
                // for gossip would bury the entries that matter. But it is
                // *logged*: "was this node ever asked, and what did it
                // say" is the first question of any rollout that stalled,
                // and until now the node kept no record of either.
                debug!(plan = %plan_id, retry, why = %message, "refused an upgrade ask");
                return Response::UpgradeState(mesh_proto::UpgradeState {
                    version: VERSION.to_string(),
                    done: false,
                    started: false,
                    refused: Some(message),
                    retry,
                });
            }
        };

        // The version answers "is this plan satisfied" for an ordinary
        // plan, and cannot for a forced one: a forced plan is published
        // *because* the version already matched and the operator disputes
        // what the binary behind it is. What answers it there is having
        // done the work, which this node remembers across its own restart
        // in one marker file — the only piece of the exchange that has to
        // survive the process replacing itself.
        if VERSION == target && (!force || self.upgrade_applied(plan_id)) {
            return Response::UpgradeState(mesh_proto::UpgradeState {
                version: VERSION.to_string(),
                done: true,
                started: false,
                refused: None,
                retry: false,
            });
        }
        // A release published into the mesh is fetched from whichever peer
        // holds it — no server, no egress, nothing to be down (§9). The
        // fetch is the *prefetch* path, started here and polled by the
        // driver's next ask: holding this request open for a hundred
        // megabytes would tie up the driver and tell it nothing meanwhile.
        if let Some(rec) = &release
            && !self.inner.content.has_bundle(&rec.bundle_id)
        {
            self.start_prefetch(rec.bundle_id);
            let progress = self.bundle_progress(&rec.bundle_id);
            return Response::UpgradeState(mesh_proto::UpgradeState {
                version: VERSION.to_string(),
                done: false,
                started: false,
                refused: Some(format!(
                    "fetching release {} from the mesh ({}/{} chunks)",
                    rec.version, progress.have_chunks, progress.total_chunks
                )),
                retry: true,
            });
        }
        {
            let mut in_flight = self.inner.upgrading.lock();
            if in_flight.is_some() {
                return Response::UpgradeState(mesh_proto::UpgradeState {
                    version: VERSION.to_string(),
                    done: false,
                    started: true,
                    refused: None,
                    retry: false,
                });
            }
            *in_flight = Some(target.clone());
        }
        // Audited on the node that acts, naming the plan — "who replaced
        // this binary and on whose authority" is exactly the question an
        // auditor asks after an upgrade goes wrong.
        audit::record(
            &self.inner.engine,
            by,
            "node.upgrade",
            &self.inner.config.node_name,
            "ok",
            &format!("{target} ({plan_id}){}", if force { " forced" } else { "" }),
            &[],
        );
        info!(
            target,
            plan = %plan_id,
            force,
            "asked to upgrade — fetching, verifying, then draining"
        );
        match self.spawn_self_upgrade(plan_id, &target, move_pending, release) {
            Ok(()) => Response::UpgradeState(mesh_proto::UpgradeState {
                version: VERSION.to_string(),
                done: false,
                started: true,
                refused: None,
                retry: false,
            }),
            Err(message) => {
                // The driver hears this, and a driver is a stream somebody
                // closes. The machine that could not start its own upgrade
                // keeps the record.
                warn!(target, plan = %plan_id, why = %message, "could not start the upgrade");
                *self.inner.upgrading.lock() = None;
                Response::Error { message }
            }
        }
    }

    /// Where a node notes the forced plan it last carried out.
    ///
    /// One line, `<plan id> <pid>`. A forced plan cannot be checked off by
    /// comparing versions — the version matched before the rollout started
    /// and matches after it, which is the whole reason the plan was forced
    /// — so the only honest answer to "did this node do it" is that the
    /// node remembers doing it. The memory has to outlive the process
    /// replacing itself, so it lands on disk beside the identity rather
    /// than in the daemon.
    pub(super) fn upgrade_applied_path(&self) -> std::path::PathBuf {
        self.inner.config.data_dir.join("upgrade-applied")
    }

    /// Did *a previous life of this node* carry out `plan_id`?
    ///
    /// The pid is what makes "previous" decidable. The note is written
    /// once the swap has landed but before the drain, so the process that
    /// wrote it is still answering asks for a few seconds — and if it
    /// counted its own note, the driver would call the node back on the
    /// new binary while the old one was still serving the answer.
    pub(super) fn upgrade_applied(&self, plan_id: mesh_proto::UpgradeId) -> bool {
        let Ok(text) = std::fs::read_to_string(self.upgrade_applied_path()) else {
            return false;
        };
        let mut fields = text.split_whitespace();
        let (Some(id), Some(pid)) = (fields.next(), fields.next()) else {
            return false;
        };
        id == plan_id.to_string() && pid != std::process::id().to_string()
    }

    /// Note that this node carried out `plan_id`, for the life of it that
    /// comes back on the new binary to read.
    pub(super) fn record_upgrade_applied(&self, plan_id: mesh_proto::UpgradeId) {
        let path = self.upgrade_applied_path();
        if let Err(e) = std::fs::write(&path, format!("{plan_id} {}", std::process::id())) {
            // Not fatal, and not silent. The upgrade itself has already
            // landed; what is lost is the node's ability to say so
            // afterwards, which a forced rollout reads as a node that
            // never came back.
            warn!(path = %path.display(), error = %e, "could not record the upgrade");
        }
    }

    /// Minisign keys this mesh trusts to have signed a release (§9),
    /// newest-trusted last. Revoked keys are gone from this list the
    /// moment the revocation merges here.
    pub(super) fn trusted_release_keys(&self) -> Vec<String> {
        self.inner.engine.read(|s| {
            s.release_keys
                .values()
                .filter(|e| e.revoked.is_none())
                .map(|e| e.key.pubkey.clone())
                .collect()
        })
    }

    /// Fetch a mesh-published release's bundle from whichever peer holds
    /// it, and lay the binary and its signature out on disk (§9).
    ///
    /// Every chunk is verified against its hash on arrival by the content
    /// store, and the *release* signature is checked by the upgrade process
    /// before anything is installed — so the peer that served the bytes is
    /// trusted with nothing at all. Returns `(binary, minisig)` paths.
    /// Lay a **locally-held** release out on disk: the binary and its
    /// signature, ready for the upgrade process (§9).
    ///
    /// Purely local and fast — obtaining the bytes is the prefetch path's
    /// job, and this runs only once they are here. Returns
    /// `(binary, minisig)`.
    pub(super) fn stage_release(
        &self,
        rec: &mesh_proto::ReleaseRecord,
    ) -> std::result::Result<
        (
            std::path::PathBuf,
            std::path::PathBuf,
            Option<std::path::PathBuf>,
        ),
        String,
    > {
        let dir = self.inner.config.data_dir.join("upgrade-staging");
        let _ = std::fs::remove_dir_all(&dir);
        self.inner
            .content
            .materialize(&rec.bundle_id, &dir)
            .map_err(|e| format!("unpacking release {}: {e}", rec.version))?;
        let binary = dir.join(&rec.artifact);
        if !binary.exists() {
            return Err(format!(
                "release {} names {} but the bundle does not contain it",
                rec.version, rec.artifact
            ));
        }
        // The digest is pinned in the signed record, so a bundle whose
        // shape changed is refused here rather than after a `rename(2)`.
        let bytes = std::fs::read(&binary).map_err(|e| format!("reading staged binary: {e}"))?;
        if *blake3::hash(&bytes).as_bytes() != rec.digest {
            return Err(format!(
                "release {} does not match the digest its record pins",
                rec.version
            ));
        }
        let minisig = dir.join(format!("{}.minisig", rec.artifact));
        std::fs::write(&minisig, &rec.minisig)
            .map_err(|e| format!("writing the release signature: {e}"))?;
        // Y6. Staged beside the binary when the mesh holds one, so the
        // child verifies it from a file exactly as it does the minisig.
        // Absent is not an error here: the upgrader decides whether one is
        // *required*, and it decides that from the trusted-key list rather
        // than from what happened to be on disk.
        let cosign = self
            .inner
            .engine
            .read(|s| {
                s.release_cosignatures
                    .get(&mesh_proto::hex32(&rec.digest))
                    .map(|c| c.cosign.armored.clone())
            })
            .map(|armored| {
                let path = dir.join(format!("{}.sshsig", rec.artifact));
                std::fs::write(&path, armored)
                    .map(|()| path)
                    .map_err(|e| format!("writing the release co-signature: {e}"))
            })
            .transpose()?;
        Ok((binary, minisig, cosign))
    }

    /// Start `meshnode upgrade --self --version <tag>` as a detached
    /// process.
    ///
    /// **Why a separate process and not this one.** The upgrade drains this
    /// daemon and waits for it to exit; a drain waits for running work; so
    /// anything driving the upgrade from *inside* the daemon's own work
    /// pipeline — a job, a session — would be waiting for itself. The CLI
    /// path M7 built is the thing that already gets this right, and it is
    /// the same binary, so this starts it rather than reimplementing it.
    ///
    /// Privilege is unchanged: the child runs as this daemon's own user and
    /// replaces the binary only if that user could already have replaced it
    /// by hand. Under the packaged systemd unit it cannot, and the refusal
    /// that comes back is M7's — which names the package manager, and is
    /// the correct answer for a package install.
    pub(super) fn spawn_self_upgrade(
        &self,
        plan_id: mesh_proto::UpgradeId,
        target: &str,
        move_pending: bool,
        release: Option<mesh_proto::ReleaseRecord>,
    ) -> std::result::Result<(), String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("this node cannot locate its own binary: {e}"))?;
        let staged = match &release {
            Some(rec) => Some(self.stage_release(rec)?),
            None => None,
        };
        let mut cmd = std::process::Command::new(&exe);
        // The data dir explicitly: the child writes the pending-upgrade
        // marker this daemon's successor reads (ENG-12), and must not find
        // a different home in whatever the environment says.
        cmd.arg("--data-dir")
            .arg(&self.inner.config.data_dir)
            .arg("upgrade")
            .arg("--self")
            .arg("--yes")
            // The daemon drains itself once the swap lands; the child must
            // not try, because it cannot authorize it (see
            // `drain_for_upgrade`).
            .arg("--no-drain")
            .arg("--version")
            .arg(target);
        // Passed explicitly rather than inherited from the environment: the
        // child is a fresh process and `[release]` is the machine owner's
        // written-down answer to "where does this box get executables
        // from", which is not a question to leave to whatever happened to
        // be exported into the daemon's env.
        if let Some((binary, minisig, cosign)) = &staged {
            cmd.arg("--from-file")
                .arg(binary)
                .arg("--minisig")
                .arg(minisig);
            if let Some(cosign) = cosign {
                cmd.arg("--from-cosign").arg(cosign);
            }
        }
        if let Some(base) = &self.inner.config.release.base {
            cmd.arg("--release-base").arg(base);
        }
        // Which keys this node will accept a release from. A local
        // `[release] pubkey` is the machine owner's written-down answer and
        // wins outright; otherwise the keys the mesh trusts, which is what
        // makes a fresh node upgradeable without anyone editing a file on
        // it. Passed explicitly for the same reason `--release-base` is:
        // the child is a fresh process and this is not a question to leave
        // to whatever happened to be in the daemon's environment.
        match &self.inner.config.release.pubkey {
            Some(pubkey) => {
                cmd.arg("--release-key").arg(pubkey);
            }
            None => {
                for key in self.trusted_release_keys() {
                    cmd.arg("--release-key").arg(key);
                }
            }
        }
        if move_pending {
            // The operator's choice, carried in the plan this node
            // re-verified — not the driver's to make on their behalf.
            cmd.arg("--move");
        }
        // **Not /dev/null.** A self-upgrade that fails — a download over a
        // cap, a signature that does not verify, a read-only install
        // directory — fails inside a process nobody is watching, on a
        // machine nobody is sitting at. Without this the only symptom is a
        // node that never comes back and a rollout that times out saying
        // nothing about why.
        let log = self.inner.config.data_dir.join("upgrade.log");
        let sink = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null())
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(sink())
            .stderr(sink());
        // The child installs *first* and drains *second* (M7's order), so
        // by the time this daemon exits the swap has already landed and
        // whatever the service manager does to the child afterwards cannot
        // leave a half-upgraded node.
        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start the upgrade on this node: {e}"))?;
        // Wait for the swap, then take this daemon down so the service
        // manager brings the new binary up. Detached from the request that
        // triggered it: the driver has already been told "started", and
        // this node finishes what it started whether or not anyone is
        // still listening.
        let node = self.clone();
        let target = target.to_string();
        tokio::spawn(async move {
            let done = tokio::task::spawn_blocking(move || {
                let mut child = child;
                child.wait()
            })
            .await;
            match done {
                Ok(Ok(status)) if status.success() => {
                    // Written before the drain and read after the restart:
                    // it is stamped with the pid that wrote it, so the
                    // process that is about to go down does not read its
                    // own note and report the plan finished a drain early.
                    node.record_upgrade_applied(plan_id);
                    node.drain_for_upgrade(&target).await
                }
                Ok(Ok(status)) => {
                    warn!(%status, "the upgrade process failed — see upgrade.log; this node \
                          keeps running the binary it has");
                    *node.inner.upgrading.lock() = None;
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "could not wait for the upgrade process");
                    *node.inner.upgrading.lock() = None;
                }
                Err(e) => {
                    warn!(error = %e, "the upgrade waiter died");
                    *node.inner.upgrading.lock() = None;
                }
            }
        });
        Ok(())
    }

    /// Roll a release across a ring, one node at a time (M24 §4).
    ///
    /// Three properties, none of them negotiable:
    ///
    /// - **Serial, with a real health gate between nodes.** The next node
    ///   does not start until the previous one is back, alive, and caught
    ///   up on the op-log. The sync-lag reading is exactly that signal.
    /// - **Never the whole mesh at once**, and never the driver
    ///   last-by-accident — it upgrades itself explicitly, last, as its own
    ///   step.
    /// - **Stop on first failure, loudly.** A partial upgrade is a
    ///   supported state: the op-log is forward-compatible precisely so a
    ///   mesh mid-upgrade keeps working, so stopping halfway is an
    ///   inconvenience, not an outage.
    pub(super) async fn upgrade_stream(
        &self,
        plan: mesh_proto::UpgradePlan,
        force: bool,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let target = normalize_release_tag(&plan.version);
        let sel = Selector::parse(&plan.selector);
        // `is_unbounded`, not `matches!(Selector::Any)`: `any|name=anvil`
        // is the whole mesh wearing a disguise (M24 §10).
        if sel.is_unbounded() {
            let _ = out
                .send(IpcResponse::Error {
                    message: "refusing `--on any`: name a ring (`tag=canary`, `tag=dc-east`) and \
                              widen it. Upgrading a whole mesh in one command is the thing this \
                              is built not to do."
                        .into(),
                })
                .await;
            return;
        }
        let me = self.node_id();
        let targets = self.inner.engine.read(|s| {
            membership::resolve_targets(s, &sel, me, |id| self.inner.gossip.liveness_of(id))
        });
        if targets.is_empty() {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("selector {:?} matches no live node", plan.selector),
                })
                .await;
            return;
        }

        // Publish the plan *before* driving it. Every node re-derives its
        // own authority from this record, so a node that has not seen it
        // yet will refuse — and the driver's first ask is what waits for
        // gossip, rather than a signature that could go stale mid-rollout.
        let plan_id = plan.plan_id;
        // Which op body carries the record *is* the forced flag: it is not
        // a field of the plan, so that adding it changed no plan's
        // encoding and older nodes go on reading ordinary ones. A node
        // with no code for the forced variant materializes nothing from
        // it, and `upgrade_one` already names that case by version.
        let body = if force {
            OpBody::UpgradePlanForce {
                plan: Box::new(plan.clone()),
            }
        } else {
            OpBody::UpgradePlanPut {
                plan: Box::new(plan.clone()),
            }
        };
        if let Err(e) = self.inner.engine.author_op(body) {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("could not publish the upgrade plan: {e}"),
                })
                .await;
            return;
        }

        // When the plan entered *this* node's log. A peer that has synced
        // past this instant has been handed the record, whatever it went on
        // to do with it — which is the reading `upgrade_one` cross-examines
        // a "not here yet" against.
        let published = Instant::now();

        // The driver goes last, as its own step: it cannot watch a rollout
        // it is in the middle of.
        let (mine, others): (Vec<MemberInfo>, Vec<MemberInfo>) =
            targets.into_iter().partition(|m| m.node_id == me);

        let mut upgraded = 0u32;
        let mut already = 0u32;
        let mut stopped_at: Option<String> = None;
        let total = others.len() + mine.len();
        let mut done_count = 0usize;

        // Waves. Serial is the default and not the only option: a thousand
        // nodes at one a minute is a rollout nobody finishes in a working
        // day. Width does not weaken the gate — a wave must be *wholly*
        // back, alive and caught up before the next one starts, so the
        // blast radius of a bad binary is one wave and never the fleet.
        let wave = plan.wave();
        for chunk in others.chunks(wave) {
            if chunk.len() > 1 {
                let _ = out
                    .send(IpcResponse::UpgradeProgress {
                        node: format!("wave of {}", chunk.len()),
                        state: "wave".into(),
                        detail: chunk
                            .iter()
                            .map(|m| m.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    })
                    .await;
            }
            // Every node in the wave is driven concurrently; the wave ends
            // when the slowest of them is healthy again.
            let results = futures::future::join_all(
                chunk
                    .iter()
                    .map(|member| self.upgrade_one(member, plan_id, &target, published, &out)),
            )
            .await;
            done_count += chunk.len();
            for (member, result) in chunk.iter().zip(results) {
                match result {
                    Ok(true) => upgraded += 1,
                    Ok(false) => already += 1,
                    Err(detail) => {
                        let _ = out
                            .send(IpcResponse::UpgradeProgress {
                                node: member.name.clone(),
                                state: "failed".into(),
                                detail,
                            })
                            .await;
                        // First failure stops the rollout, and it stops it
                        // at the wave boundary: the rest of this wave has
                        // already been driven, and nothing beyond it is.
                        stopped_at.get_or_insert_with(|| member.name.clone());
                    }
                }
            }
            if stopped_at.is_some() {
                break;
            }
        }

        let mut driver_restarting = false;
        if stopped_at.is_none() && !mine.is_empty() {
            done_count += 1;
            // This node last, explicitly. It will drain and go down, so
            // this is the last frame the caller sees from it — and it is
            // the one step the driver cannot verify, because verifying
            // would mean being alive on the other side of its own restart.
            let (state, detail) = match self.serve_upgrade_self(plan_id) {
                Response::UpgradeState(s) if s.done => {
                    already += 1;
                    ("already", format!("already runs {target}"))
                }
                Response::UpgradeState(s) if s.refused.is_some() => {
                    stopped_at = Some(self.inner.config.node_name.clone());
                    ("failed", s.refused.unwrap_or_default())
                }
                Response::UpgradeState(_) => {
                    driver_restarting = true;
                    (
                        "upgrading",
                        format!(
                            "upgrading itself last, as its own step — this daemon drains and \
                             restarts as {target}. Confirm with `meshnode upgrade --status`."
                        ),
                    )
                }
                Response::Error { message } => {
                    stopped_at = Some(self.inner.config.node_name.clone());
                    ("failed", message)
                }
                other => {
                    stopped_at = Some(self.inner.config.node_name.clone());
                    ("failed", format!("unexpected answer: {other:?}"))
                }
            };
            let _ = out
                .send(IpcResponse::UpgradeProgress {
                    node: self.inner.config.node_name.clone(),
                    state: state.into(),
                    detail,
                })
                .await;
        }

        let _ = out
            .send(IpcResponse::UpgradeDone {
                upgraded,
                already,
                driver_restarting,
                remaining: u32::try_from(total.saturating_sub(done_count)).unwrap_or(u32::MAX),
                stopped_at,
            })
            .await;
    }

    /// How many jobs the mesh believes are running on `node`.
    ///
    /// From the replicated summaries, which every member holds, so a
    /// rollout driver can watch another node's drain shrink without
    /// asking it anything the protocol does not already carry (ENG-26).
    pub(super) fn jobs_running_on(&self, node: &NodeId) -> usize {
        self.inner.engine.read(|s| {
            s.jobs
                .values()
                .filter(|j| j.executor == *node && !j.state.is_terminal())
                .count()
        })
    }

    /// One node's step: ask, then wait for it to come back healthy.
    ///
    /// `Ok(true)` = upgraded, `Ok(false)` = already there, `Err` = stop the
    /// rollout.
    pub(super) async fn upgrade_one(
        &self,
        member: &MemberInfo,
        plan_id: mesh_proto::UpgradeId,
        target: &str,
        published: Instant,
        out: &mpsc::Sender<IpcResponse>,
    ) -> std::result::Result<bool, String> {
        let name = member.name.clone();
        // The first ask is also what waits for the plan to reach this node:
        // it refuses until it has the record, so a few seconds of gossip
        // lag reads as a retry rather than as a failure.
        // Two clocks (ENG-26). `stall` is reset by progress; `ceiling`
        // never is.
        let ceiling = Instant::now() + UPGRADE_NODE_MAX;
        let mut stall = Instant::now() + UPGRADE_NODE_STALL;
        let mut waiting_on: Option<String> = None;
        let mut last_said = Instant::now();
        let mut attempts = 0u32;
        let began = Instant::now();
        let started = loop {
            attempts += 1;
            match self.ask_upgrade(member, plan_id).await {
                Ok(state) if state.done => {
                    let _ = out
                        .send(IpcResponse::UpgradeProgress {
                            node: name,
                            state: "already".into(),
                            detail: format!("already runs {target}"),
                        })
                        .await;
                    return Ok(false);
                }
                Ok(_) => break true,
                // Terminal refusals stop the rollout now. Retrying one
                // until the deadline would turn a fact the node stated in
                // one round trip into ten minutes of silence.
                Err(f) if f.terminal => return Err(f.why),
                Err(f) => {
                    // "Not here yet" is the one refusal the op-log can
                    // contradict. If this peer has synced from us *since*
                    // the plan was published and took everything we had,
                    // then it holds the record and cannot read it — its
                    // build has no code for that entity kind, so it carries
                    // the op forward for everyone else and materializes
                    // nothing (F0 §3.2). Ten more minutes of asking will
                    // not change a build. Say so, name the hop out of it,
                    // and stop: this is the same bootstrap case as a peer
                    // below `FLEET_MINOR`, arriving one layer down.
                    if f.pending
                        && peer_holds_the_record(
                            self.inner.gossip.sync_lag(&member.node_id),
                            published,
                        )
                    {
                        let build = f
                            .peer_version
                            .as_deref()
                            .map(|v| format!("meshnode {v}"))
                            .unwrap_or_else(|| "a build".to_string());
                        return Err(format!(
                            "{name} runs {build}, which has no code for the rollout record this \
                             mesh publishes — it has been handed the record and cannot read it, \
                             so waiting will not help. Upgrade it once by hand with `meshnode \
                             upgrade --self`, and every rollout after that can drive it."
                        ));
                    }
                    if Instant::now() >= stall || Instant::now() >= ceiling {
                        return Err(f.why);
                    }
                    // Say what it is waiting on when the answer changes —
                    // a node pulling a hundred megabytes off its peers
                    // should look like progress, not like a hang, and
                    // repeating the same line every three seconds is
                    // neither.
                    //
                    // But silence is not the alternative. A node stuck on
                    // an *unchanging* answer used to emit one line and then
                    // nothing for ten minutes, which reads identically to a
                    // node nobody is asking — the two states an operator
                    // most needs to tell apart. So an unchanging answer is
                    // repeated on a slow heartbeat, carrying how long it
                    // has been saying it.
                    let changed = waiting_on.as_deref() != Some(f.why.as_str());
                    // A new answer is progress: the node is working through
                    // something rather than stuck on it.
                    if changed {
                        stall = Instant::now() + UPGRADE_NODE_STALL;
                    }
                    if changed || last_said.elapsed() >= UPGRADE_STILL_WAITING {
                        let detail = if changed {
                            f.why.clone()
                        } else {
                            format!(
                                "{} — still, after {}s and {attempts} asks",
                                f.why,
                                began.elapsed().as_secs()
                            )
                        };
                        let _ = out
                            .send(IpcResponse::UpgradeProgress {
                                node: name.clone(),
                                state: "waiting".into(),
                                detail,
                            })
                            .await;
                        waiting_on = Some(f.why);
                        last_said = Instant::now();
                    }
                    tokio::time::sleep(UPGRADE_POLL).await;
                }
            }
        };
        if started {
            let _ = out
                .send(IpcResponse::UpgradeProgress {
                    node: name.clone(),
                    state: "asked".into(),
                    detail: format!("fetching and verifying {target}, then draining"),
                })
                .await;
        }

        // The health gate. Not "did the request return" — back, alive, and
        // caught up on the op-log, which is the only reading that says the
        // node is *participating* again rather than merely listening.
        // What the node still has to finish before it can restart. A drain
        // that is getting shorter is progress, however long it takes, and
        // this is the only reading that tells it apart from a hang.
        let mut draining = self.jobs_running_on(&member.node_id);
        loop {
            tokio::time::sleep(UPGRADE_POLL).await;
            let left = self.jobs_running_on(&member.node_id);
            if left < draining {
                draining = left;
                stall = Instant::now() + UPGRADE_NODE_STALL;
            }
            if Instant::now() >= ceiling {
                return Err(format!(
                    "{name} has been upgrading for {}h and is still not on {target} — the rollout \
                     stops here so somebody can look at it",
                    UPGRADE_NODE_MAX.as_secs() / 3600
                ));
            }
            if Instant::now() >= stall {
                return Err(format!(
                    "{name} has made no progress towards {target} for {}s ({left} job(s) still \
                     running there) — the rollout stops here so somebody can look at it",
                    UPGRADE_NODE_STALL.as_secs()
                ));
            }
            // A node mid-restart refuses connections; that is expected and
            // is not the failure. Only the deadline is — or the node saying
            // it cannot, as one that rolled the release back does (ENG-12).
            let state = match self.ask_upgrade(member, plan_id).await {
                Ok(state) => state,
                Err(f) if f.terminal => return Err(f.why),
                Err(_) => continue,
            };
            if !state.done {
                continue;
            }
            if self.inner.gossip.liveness_of(&member.node_id) != Liveness::Alive {
                continue;
            }
            if self
                .inner
                .gossip
                .sync_lag(&member.node_id)
                .map(|lag| lag.behind)
                == Some(0)
            {
                let _ = out
                    .send(IpcResponse::UpgradeProgress {
                        node: name,
                        state: "back".into(),
                        detail: format!("runs {target}, alive, and caught up"),
                    })
                    .await;
                return Ok(true);
            }
        }
    }

    pub(super) async fn ask_upgrade(
        &self,
        member: &MemberInfo,
        plan_id: mesh_proto::UpgradeId,
    ) -> std::result::Result<mesh_proto::UpgradeState, AskFailure> {
        let conn = self
            .inner
            .transport
            .member_conn(member.node_id, &member.addrs)
            .await
            // A node mid-restart refuses connections, and that is the
            // expected middle of an upgrade rather than a failure of it.
            .map_err(|e| AskFailure::transient(e.to_string()))?;
        let minor = self
            .inner
            .transport
            .peer_minor(&member.node_id)
            .unwrap_or(0);
        if minor < mesh_proto::FLEET_MINOR {
            // Terminal, and knowable in one round trip. This is the
            // bootstrap case every fleet hits exactly once: a build from
            // before fleet upgrade existed has no code for the request, so
            // the first hop onto a drivable build is always by hand.
            return Err(AskFailure::terminal(format!(
                "{} runs meshnode too old to be driven this way (protocol 1.{minor}; this needs \
                 1.{}). Upgrade it once by hand with `meshnode upgrade --self`, and every \
                 rollout after that can drive it.",
                member.name,
                mesh_proto::FLEET_MINOR
            )));
        }
        match self
            .inner
            .transport
            .request_one(&conn, &Request::UpgradeSelf { plan_id })
            .await
        {
            // A refusal is the node's own answer, and it says whether
            // asking again could ever change it.
            Ok(Response::UpgradeState(state)) => match &state.refused {
                Some(why) => Err(AskFailure {
                    why: format!("{}: {why}", member.name),
                    terminal: !state.retry,
                    pending: state.retry,
                    peer_version: Some(state.version.clone()),
                }),
                None => Ok(state),
            },
            Ok(Response::Error { message }) => {
                Err(AskFailure::terminal(format!("{}: {message}", member.name)))
            }
            Ok(other) => Err(AskFailure::terminal(format!(
                "{}: unexpected answer: {other:?}",
                member.name
            ))),
            Err(e) => Err(AskFailure::transient(format!("{}: {e}", member.name))),
        }
    }

    /// Releases this mesh holds, newest version first (M24 §9).
    pub(super) fn release_rows(&self) -> Vec<mesh_proto::ReleaseRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let me = self.node_id();
        self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::ReleaseRow> = s
                .releases
                .values()
                .map(|r| {
                    let bundle = s.bundles.get(&r.bundle_id);
                    mesh_proto::ReleaseRow {
                        version: r.version.clone(),
                        bundle_id: r.bundle_id,
                        artifact: r.artifact.clone(),
                        total_size: bundle.map(|b| b.meta.total_size).unwrap_or(0),
                        holders: bundle.map(|b| b.holders.len()).unwrap_or(0),
                        local: bundle.is_some_and(|b| b.holders.contains(&me)),
                        published_by: rbac::username_of(s, &founder, &r.published_by),
                        published_at_micros: r.published_at_micros,
                        arch: crate::release::arch_of(r).unwrap_or("").to_string(),
                        digest: mesh_proto::hex32(&r.digest),
                        // Y6. Read from the *verified* record: a
                        // co-signature the mesh could not attribute to a
                        // trusted key was never stored, so this column can
                        // only ever name a key the mesh actually trusts.
                        cosigned_by: s
                            .release_cosignatures
                            .get(&mesh_proto::hex32(&r.digest))
                            .map(|c| c.signed_by_key.clone()),
                        cosign_user_present: s
                            .release_cosignatures
                            .get(&mesh_proto::hex32(&r.digest))
                            .is_some_and(|c| c.user_present),
                    }
                })
                .collect();
            rows.sort_by(|a, b| {
                version_cmp(&b.version, &a.version).then_with(|| a.arch.cmp(&b.arch))
            });
            rows
        })
    }

    /// What this node verifies releases against (M24 §9).
    pub(super) fn release_key_view(&self) -> mesh_proto::ReleaseKeyView {
        let founder = self.inner.engine.genesis().body.founder_user;
        let local_override = self.inner.config.release.pubkey.clone();
        let keys = self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::ReleaseKeyRow> = s
                .release_keys
                .values()
                .map(|e| mesh_proto::ReleaseKeyRow {
                    pubkey: e.key.pubkey.clone(),
                    label: e.key.label.clone(),
                    added_by: rbac::username_of(s, &founder, &e.key.added_by),
                    added_at_micros: e.key.added_at_micros,
                    revoked: e.revoked.is_some(),
                    in_use_here: e.revoked.is_none() && local_override.is_none(),
                })
                .collect();
            rows.sort_by_key(|r| std::cmp::Reverse(r.added_at_micros));
            rows
        });
        mesh_proto::ReleaseKeyView {
            keys,
            local_override,
            embedded_is_placeholder: embedded_release_key_is_placeholder(),
        }
    }

    /// Live and recent plans, newest first (M24 §4).
    pub(super) fn upgrade_plan_rows(&self) -> Vec<mesh_proto::UpgradePlanRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::UpgradePlanRow> = s
                .upgrade_plans
                .values()
                .map(|e| mesh_proto::UpgradePlanRow {
                    plan_id: e.plan.plan_id,
                    selector: e.plan.selector.clone(),
                    version: e.plan.version.clone(),
                    move_pending: e.plan.move_pending,
                    created_by: rbac::username_of(s, &founder, &e.plan.created_by),
                    created_at_micros: e.plan.created_at_micros,
                    expires_at_micros: e.plan.expires_at(),
                    cancelled: e.cancelled.is_some(),
                    live: e.is_live_at(now),
                    force: e.force,
                })
                .collect();
            rows.sort_by_key(|r| std::cmp::Reverse(r.created_at_micros));
            rows
        })
    }
}
