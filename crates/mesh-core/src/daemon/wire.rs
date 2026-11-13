//! The peer request dispatcher: what one node will do for another.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    pub(super) async fn handle_wire(&self, event: RequestEvent) {
        let RequestEvent {
            peer,
            request,
            reply,
        } = event;
        // -- the second door (S1) -------------------------------------------
        //
        // Admission lets an unknown peer connect on purpose: a node whose
        // `MemberAdd` is still in flight would otherwise be locked out of the
        // gossip that would admit it. But `mesh_id` — the only other thing
        // admission checks — is not a secret. A `Probe` connection is
        // answered to anyone and its ack carries the mesh id verbatim,
        // because that is what `meshnode ping`'s `same_mesh` field reads.
        //
        // So a stranger could learn the id with one ping and then ask for
        // everything: the whole replicated log (users, grants, inventory,
        // policies, the audit trail), any job's live output, archive pages,
        // bundle content — and `CancelJob`, which is a *write*. The IPC path
        // gates all of that behind `require_role`; this one did not.
        //
        // The check belongs here rather than at admission for exactly the
        // reason admission is permissive: by the time a request arrives,
        // "gossip may be mid-flight" is no longer a reason to answer, and
        // the handshakes that a genuine non-member runs are enumerated in
        // `Request::requires_membership`.
        let is_member = self
            .inner
            .engine
            .read(|s| s.is_active_member(&peer.node_id));
        // Inbound member traffic is evidence of life — from a *member*.
        // This used to run before the membership check, so any key that
        // opened a member connection got a liveness entry, and every entry
        // rides every sync: about 100,000 junk keys pushed syncs past the
        // frame cap and the whole mesh declared itself dead (SEC-2).
        if peer.purpose == ConnPurpose::Member && is_member {
            self.inner.gossip.mark_alive(&peer.node_id);
        }
        if request.requires_membership() && !is_member {
            debug!(
                peer = %peer.node_id.short(),
                request = request.name(),
                "refusing a wire request from a non-member"
            );
            let _ = reply
                .send(Response::Error {
                    message: "not a member of this mesh".into(),
                })
                .await;
            return;
        }
        match request {
            Request::Ping(p) => {
                let _ = reply.send(Response::Pong(p)).await;
            }
            // A V1 Join means a pre-1.7 joiner: a 1.7 one sends JoinV2.
            // Check before authoring, so a refusal spends no invite use and
            // leaves no half-member behind (F0 §3.3).
            Request::Join(req) => {
                if let Some(message) = membership::legacy_join_refusal(&self.inner.engine) {
                    let _ = reply.send(Response::Error { message }).await;
                    return;
                }
                self.learn_invite(&req.token).await;
                let result = membership::handle_join(
                    &self.inner.engine,
                    peer.node_id,
                    peer.remote_addr.ip(),
                    &req,
                );
                let resp = match result {
                    // A pre-1.7 joiner: hand it the prefix of the snapshot
                    // it can decode (F0 §3.3).
                    Ok(accept) => {
                        let downgraded = membership::downgrade_join(&accept);
                        match membership::oversized_snapshot_refusal(&downgraded, peer.minor) {
                            Some(message) => Response::Error { message },
                            None => Response::JoinAccept(Box::new(downgraded)),
                        }
                    }
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            // A 1.7–1.9 joiner: one frame is all it can read, so it gets
            // one — and a mesh with more history than a frame holds gets a
            // refusal that names the version rather than a codec error.
            Request::JoinV2(req) => {
                self.learn_invite(&req.token).await;
                let result = membership::handle_join(
                    &self.inner.engine,
                    peer.node_id,
                    peer.remote_addr.ip(),
                    &req,
                );
                let resp = match result {
                    Ok(accept) => match membership::oversized_snapshot_refusal(&accept, peer.minor)
                    {
                        Some(message) => Response::Error { message },
                        None => Response::JoinAcceptV2(Box::new(accept)),
                    },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            // A 1.10 joiner: the first instalment, and it comes back for
            // the rest with `JoinFetch`.
            Request::JoinV3(req) => {
                self.learn_invite(&req.token).await;
                let result = membership::handle_join_chunked(
                    &self.inner.engine,
                    peer.node_id,
                    peer.remote_addr.ip(),
                    &req,
                );
                let resp = match result {
                    Ok(chunk) => Response::Snapshot(Box::new(chunk)),
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            // The continuation leg. It carries no invite — the invite is
            // spent by the time the first chunk goes out — so the check is
            // membership itself: `JoinV3` authored the joiner's `MemberAdd`
            // before answering, and anyone who never presented a token is
            // not in this mesh and gets nothing.
            Request::JoinFetch { vv } => {
                let is_member = self
                    .inner
                    .engine
                    .read(|s| s.is_active_member(&peer.node_id));
                let resp = if is_member {
                    match membership::snapshot_chunk(&self.inner.engine, &vv, false) {
                        Ok(chunk) => Response::Snapshot(Box::new(chunk)),
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    }
                } else {
                    Response::Error {
                        message: "not a member of this mesh — present an invite first".into(),
                    }
                };
                let _ = reply.send(resp).await;
            }
            // A pre-1.7 requester. It cannot decode an op body it has no
            // variant for, and postcard decodes `Vec<Op>` atomically — so
            // one such op in the batch would fail its *whole* response.
            // Withhold those ops and truncate the advertised version vector
            // to just before the first one, per author: the peer is told it
            // is caught up as far as it can actually be served, stops
            // retrying a batch that can never land, and converges on the
            // subset of state it has code for (F0 §3.3).
            Request::Sync(sync) => {
                if !is_member {
                    let _ = reply.send(introduce_v1(&self.inner.engine)).await;
                    return;
                }
                self.inner.gossip.merge_remote(&sync.liveness);
                self.inner.gossip.mark_alive(&peer.node_id);
                let resp = match self.inner.engine.missing_for(&sync.vv) {
                    Ok((envelopes, more)) => {
                        let (ops, cut) = mesh_proto::downgrade_for_v1(&envelopes);
                        let mut vv = self.inner.engine.version_vector();
                        let lag = (!cut.is_empty()).then(|| crate::gossip::LegacyLag {
                            minor: peer.minor,
                            authors: cut.len(),
                            ops: cut
                                .iter()
                                .map(|(author, first)| {
                                    vv.iter()
                                        .find(|(a, _)| a == author)
                                        .map(|(_, ours)| {
                                            ours.saturating_sub(first.saturating_sub(1))
                                        })
                                        .unwrap_or(0)
                                })
                                .sum(),
                        });
                        // Sync progress, measured before the downgrade
                        // truncation: what this peer is missing from the
                        // *whole* log is the honest number, and the part it
                        // can never be given is already reported separately
                        // as `stale`.
                        {
                            let ours = self.inner.engine.version_vector();
                            let (behind, authors) = mesh_proto::vv_lag(&ours, &sync.vv);
                            self.inner.gossip.note_sync_lag(
                                peer.node_id,
                                behind,
                                mesh_proto::vv_total(&ours),
                                authors,
                            );
                        }
                        mesh_proto::truncate_vv(&mut vv, &cut);
                        if let Some(lag) = lag {
                            debug!(
                                peer = %peer.node_id.short(),
                                minor = peer.minor,
                                authors = lag.authors,
                                ops = lag.ops,
                                "withholding ops a pre-1.7 peer cannot decode"
                            );
                        }
                        self.inner.gossip.note_legacy_lag(peer.node_id, lag);
                        let progressed = !ops.is_empty();
                        Response::Sync(mesh_proto::SyncResponse {
                            ops,
                            // Only invite another round trip if this one
                            // moved the peer forward; otherwise it would
                            // loop on a batch that can never land.
                            more: more && progressed,
                            vv,
                            liveness: self.inner.gossip.snapshot(),
                        })
                    }
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::SyncV2(sync) => {
                if !is_member {
                    let _ = reply.send(introduce_v2(&self.inner.engine)).await;
                    return;
                }
                self.inner.gossip.merge_remote(&sync.liveness);
                self.inner.gossip.mark_alive(&peer.node_id);
                // A peer that speaks V2 is owed nothing; clear any lag it
                // accrued before it upgraded.
                self.inner.gossip.note_legacy_lag(peer.node_id, None);
                // **Built once for the whole request** (**M-1**): the lag
                // report, the op selection and the response all describe
                // the same instant, so rebuilding it three times was both
                // wasted work and a way for the three to disagree.
                let ours = self.inner.engine.version_vector();
                {
                    let (behind, authors) = mesh_proto::vv_lag(&ours, &sync.vv);
                    self.inner.gossip.note_sync_lag(
                        peer.node_id,
                        behind,
                        mesh_proto::vv_total(&ours),
                        authors,
                    );
                }
                let resp = match self.inner.engine.missing_for_with(&ours, &sync.vv) {
                    Ok((ops, more)) => Response::SyncV2(mesh_proto::SyncResponseV2 {
                        ops,
                        more,
                        vv: ours,
                        liveness: self.inner.gossip.snapshot(),
                    }),
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::PushOps { ops } => {
                let resp = match self.inner.engine.ingest_v1(ops) {
                    Ok(_) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::PushOpsV2 { ops } => {
                // **Off the reactor** (**H-3**). Ingest is Ed25519
                // verification (50–100 µs an op) plus a redb write, and a
                // sync batch is up to 512 ops — so this ran tens of
                // milliseconds of CPU and a disk write on the very worker
                // that answers every other request, up to eight times a
                // gossip round.
                let engine = self.inner.engine.clone();
                let resp = match tokio::task::spawn_blocking(move || engine.ingest(ops)).await {
                    Ok(Ok(_)) => Response::Ok,
                    Ok(Err(e)) => Response::Error {
                        message: e.to_string(),
                    },
                    Err(e) => Response::Error {
                        message: format!("ingest task failed: {e}"),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::SubmitJob(spec) => {
                // Someone routed a job to us to execute. Ensure any required
                // bundle is present locally first (fetch from holders).
                let resp = match self.accept_job(spec.clone()).await {
                    Ok(()) => Response::JobAccepted {
                        job_id: spec.job_id,
                        executor: self.node_id(),
                    },
                    // Backpressure is a definitive refusal, distinct from an
                    // error: the submitter spills over (MASTERPLAN §10.2).
                    Err(CoreError::Busy { running, queued }) => Response::Busy { running, queued },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            // V1: a job id and nothing else, so the only authorization
            // available is the membership check above. Still served, for two
            // reasons — a pre-`AUTHZ_MINOR` peer has no other request to
            // send, and gang teardown is one node stopping member jobs it
            // placed itself, with no person behind it to name. A human's
            // cancel travels as `CancelJobV2`.
            Request::CancelJob { job_id } => {
                // Unsigned, so only for the nodes that stop work they
                // placed; anybody else's cancel carries a person's request
                // as `CancelJobV2` (SEC-15).
                if self.inner.jobs.has_record(&job_id)
                    && !self.may_cancel_unsigned(peer.node_id, &job_id)
                    && !self.dispatched_by_or_learn(peer.node_id, &job_id).await
                {
                    let _ = reply
                        .send(Response::Error {
                            message: "an unsigned cancel is accepted only from the node that \
                                      placed the job"
                                .into(),
                        })
                        .await;
                    return;
                }
                let resp = match self.inner.jobs.cancel(&job_id) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            // V2 (S1): the canceller's own envelope, verified *here*.
            //
            // The node the person talked to has already applied
            // `require_cancel` against its own state; this applies it again
            // against ours, which is the view that actually owns the job.
            // The peer is a courier — it cannot mint this envelope, and it
            // cannot widen what the signature says.
            Request::CancelJobV2 { env } => {
                let resp = self.serve_cancel_v2(*env).await;
                let _ = reply.send(resp).await;
            }
            // V1: served to the node that dispatched the job and to the
            // owners of its fan-out or gang — the nodes that stream a run's
            // output to the person who started it. Anybody else's read has
            // to carry that person's request (SEC-15).
            Request::FollowJob {
                job_id,
                stdout_offset,
                stderr_offset,
                follow,
            } => {
                if self.inner.jobs.has_record(&job_id)
                    && !self.dispatched_by_or_learn(peer.node_id, &job_id).await
                {
                    let _ = reply
                        .send(Response::Error {
                            message: format!(
                                "{} serves a job's output to the node that dispatched it, or to \
                                 a person's signed request",
                                self.inner.config.node_name
                            ),
                        })
                        .await;
                    return;
                }
                self.serve_follow(job_id, stdout_offset, stderr_offset, follow, reply)
                    .await;
            }
            Request::FollowJobV2 {
                job_id,
                stdout_offset,
                stderr_offset,
                follow,
                env,
            } => {
                if let Err(message) = self.verify_log_read(&env, &job_id, peer.node_id) {
                    let _ = reply.send(Response::Error { message }).await;
                    return;
                }
                self.serve_follow(job_id, stdout_offset, stderr_offset, follow, reply)
                    .await;
            }
            // -- M31 B3: reading history back --------------------------
            //
            // Serving a peer's browse. The archive is read **out of
            // process** by `meshware-archive`, because `meshnode` must not
            // link DataFusion (C2) — the same rule that keeps the node one
            // small binary, applied to the one place B3 needs Parquet.
            Request::ArchivePage { query } => {
                let dir = self.inner.config.layout().archive_dir();
                let has_archive = crate::archive_read::has_archive(&dir);
                let rows = if has_archive {
                    crate::archive_read::query(&dir, &query)
                        .await
                        .unwrap_or_else(|e| {
                            warn!(error = %e, "serving an archive page failed");
                            Vec::new()
                        })
                } else {
                    Vec::new()
                };
                let reply_page = rows_into_reply(&rows, has_archive);
                let _ = reply
                    .send(Response::ArchivePage(Box::new(reply_page)))
                    .await;
            }
            // Fetching one payload for a peer. **This node is the one with
            // the route**, which is the whole reason the request exists:
            // Director on a laptop with no path to the warehouse still
            // opens the log, and the read is governed exactly like the
            // write that put it there.
            Request::ReplayPayload {
                job_id,
                sink,
                key,
                from_end,
                limit,
                search,
            } => {
                let page = self
                    .replay_payload_locally(
                        &job_id,
                        &sink,
                        &key,
                        &crate::sinks::PayloadWindow {
                            from_end,
                            limit,
                            search,
                        },
                    )
                    .await;
                let _ = reply
                    .send(Response::PayloadWindow(Box::new(
                        mesh_proto::PayloadWindowReply {
                            lines: page.lines,
                            more_before: page.more_before,
                            status: page.status,
                        },
                    )))
                    .await;
            }
            Request::GetManifest { bundle_id } => {
                let resp = match self.inner.content.load_manifest(&bundle_id) {
                    Ok(Some(m)) => {
                        // A peer pulling this is the mesh saying the bundle
                        // still matters, so it counts as a use here too.
                        // Per *manifest*, not per chunk: one fetch is one
                        // use, and the chunk path runs a thousand times for
                        // the same bundle.
                        self.inner.content.touch(&bundle_id);
                        Response::Manifest(Box::new(m))
                    }
                    Ok(None) => Response::Error {
                        message: format!("manifest not held: {}", bundle_id.short()),
                    },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::GetChunk { hash } => {
                // Bundles first, then ephemeral objects. A chunk hash is a
                // global content address, so one fetch path serves both
                // classes and the fetcher never has to say which it wants
                // (M9 §5.5).
                //
                // **Off the async workers** (ENG-26). A chunk is up to
                // `content::CHUNK_SIZE` read straight off the disk, and
                // this is the request a fleet-wide bundle push makes
                // thousands of times — the 1 GB-to-10-peers proof point is
                // ten thousand of these. Every one of them held a runtime
                // thread through a synchronous read, so the distribution
                // path competed with the gossip and job supervision
                // running on the same threads.
                let content = self.inner.content.clone();
                let objects = self.inner.objects.clone();
                let resp = tokio::task::spawn_blocking(move || match content.read_chunk(&hash) {
                    Ok(Some(data)) => Response::Chunk { data },
                    Ok(None) => Response::Chunk {
                        data: objects.read_chunk(&hash).unwrap_or_default(),
                    },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                })
                .await
                .unwrap_or_else(|e| Response::Error {
                    message: format!("chunk read panicked: {e}"),
                });
                let _ = reply.send(resp).await;
            }
            // Pair* requests only reach nodes waiting in `join --pair`;
            // members refuse Pair connections at admission, so this arm is
            // unreachable in practice.
            Request::PairInit(_)
            | Request::PairReveal { .. }
            | Request::PairConfirm
            | Request::PairComplete(_)
            | Request::PairCompleteV2(_)
            | Request::PairCompleteV3(_) => {
                let _ = reply
                    .send(Response::Error {
                        message: "this node is a mesh member, not a pairing candidate".into(),
                    })
                    .await;
            }
            // WAN requests (MASTERPLAN M7) are answered in the transport
            // (Observe) or handed to the relay hub before dispatch; they
            // never reach the daemon. Defensive stub.
            Request::Observe
            | Request::RelayRegister
            | Request::RelayOpen { .. }
            | Request::PunchRequest { .. } => {
                let _ = reply
                    .send(Response::Error {
                        message: "WAN request routed past the transport".into(),
                    })
                    .await;
            }
            // -- bundle prefetch (M24 §3.2) ----------------------------------
            //
            // "Fetch this now, and tell me how far you have got." Idempotent
            // by construction: the reading is always sent, and the fetch is
            // only started if one is not already running — so the asker
            // polls by re-sending, and two askers are one fetch.
            //
            // The fetch itself outlives this request. A node asked to
            // prefetch a 200 MB environment finishes it whether or not the
            // person who asked is still watching, exactly as a drain does.
            Request::BundleFetch { bundle_id } => {
                self.start_prefetch(bundle_id);
                let _ = reply
                    .send(Response::BundleProgress(self.bundle_progress(&bundle_id)))
                    .await;
            }
            // -- fleet upgrade (M24 §4) ---------------------------------------
            //
            // Authorized by the *operator's* envelope, re-verified here,
            // against this node's own state, naming this node. The driver
            // is a courier.
            Request::UpgradeSelf { plan_id } => {
                let resp = self.serve_upgrade_self(plan_id);
                let _ = reply.send(resp).await;
            }
            // -- gang scheduling (C0-C9 §8.2) --------------------------------
            //
            // The two-phase pair, served here because only the daemon has
            // the job scheduler and a lease is a decision the scheduler
            // makes. Every arm is idempotent by gang id, which is what
            // makes an origin's retry and a successor's re-drive safe.
            Request::GangOffer {
                gang,
                ticket,
                reqs,
                members,
                ttl_secs,
            } => {
                let quarantined = self.quarantined_here();
                let resp = if self.is_draining() || quarantined.is_some() {
                    // The same rule `accept_job` follows: a draining (or
                    // quarantined, SEC-16) node promises nothing new.
                    // Refusing here rather than at commit is what keeps the
                    // gang from holding N-1 reservations for a TTL.
                    Response::GangOffer(mesh_proto::GangOfferReply::Refused {
                        reason: mesh_proto::GangRefusal::NotAccepting {
                            reason: quarantined.unwrap_or_else(|| "this node is draining".into()),
                        },
                    })
                } else {
                    // What one member needs in the way of devices is read
                    // from the gang's **replicated plan**, not from the
                    // offer (C5 §9.1): `Requirements` is embedded in this
                    // very request and a struct in a wire message may never
                    // grow a field. The origin authors the plan before the
                    // first offer goes out, so the record is there to read;
                    // a plan that has not gossiped here yet means no device
                    // reservation, and the member job's own admission then
                    // does the check.
                    let devices = self.inner.engine.read(|s| {
                        s.gangs
                            .get(&gang)
                            .and_then(|e| mesh_proto::DeviceRequest::from_env(&e.plan.env))
                    });
                    Response::GangOffer(
                        self.inner
                            .jobs
                            .offer_lease(gang, ticket, reqs, members, ttl_secs, devices),
                    )
                };
                let _ = reply.send(resp).await;
            }
            Request::GangCommit { gang } => {
                // `Ok` means the reservation survived to here and the
                // member job may now arrive; an error means it did not, and
                // the origin must release the rest rather than start a
                // partial gang. That distinction is the entire commit.
                let resp = match self.inner.jobs.commit_lease(&gang) {
                    Some(_) => Response::Ok,
                    None => Response::Error {
                        message: format!(
                            "no live reservation for {} here — it expired or was \
                             yielded to a senior gang",
                            gang.short()
                        ),
                    },
                };
                let _ = reply.send(resp).await;
            }
            Request::GangRelease { gang } => {
                self.inner.jobs.release_lease(&gang);
                let _ = reply.send(Response::Ok).await;
            }
            // Live pressure (M9 §5.2). Answered here rather than in the
            // transport because only the daemon knows the executor's queue
            // depth; the probes themselves are a handful of /proc reads.
            Request::Pressure => {
                let _ = reply.send(Response::Pressure(self.pressure())).await;
            }
            // The same question with the device half attached (C5 P5). An
            // appended pair rather than a grown `PressureSnapshot`, which
            // is embedded in `Response::Pressure` and may never grow a
            // field — so a 1.13 node asks this one and learns both halves,
            // and a peer below `ACCEL_MINOR` is asked the V1 question and
            // its device telemetry stays unknown.
            Request::PressureV2 => {
                let _ = reply
                    .send(Response::PressureV2(Box::new(mesh_proto::PressureV2 {
                        base: self.pressure(),
                        devices: self
                            .inner
                            .jobs
                            .device_pressure()
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                    })))
                    .await;
            }
            // The same question about the whole mesh rather than about
            // this node (`telemetry_entries`). A pre-1.35 peer, which
            // decodes readings by position, gets them without their
            // vendor, exactly as it always did.
            Request::Telemetry => {
                let entries = self
                    .telemetry_entries()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                let _ = reply.send(Response::Telemetry(entries)).await;
            }
            Request::TelemetryV2 => {
                let _ = reply
                    .send(Response::TelemetryV2(self.telemetry_entries()))
                    .await;
            }
            // Retired with the host inventory. An older peer that still asks
            // is told so, the way `ServiceConnect` is.
            Request::HostProbes { .. } => {
                let _ = reply
                    .send(Response::Error {
                        message: INVENTORY_RETIRED.into(),
                    })
                    .await;
            }
            // -- the fabric (M17 §3.3) ---------------------------------------
            //
            // Never reached: the transport answers `SessionOpen` itself,
            // because the handler needs *both* halves of the stream and a
            // `RequestEvent` only carries a reply channel. Here so the match
            // stays exhaustive and so a future refactor that moves the arm
            // out of the transport fails loudly rather than silently
            // hanging every shell in the mesh.
            Request::SessionOpen(_) => {
                let _ = reply
                    .send(Response::Error {
                        message: "a session is served in the transport, not the daemon".into(),
                    })
                    .await;
            }
            // Never reached either: the transport refuses the retired mesh
            // proxy's request itself.
            Request::ServiceConnect { .. } => {
                let _ = reply
                    .send(Response::Error {
                        message: crate::transport::SERVICE_CONNECT_RETIRED.into(),
                    })
                    .await;
            }
            // -- parallel compute (M9 §5.4) ----------------------------------
            Request::ParallelOffer { plan, lanes } => {
                let resp = self.accept_parallel_offer(peer.node_id, *plan, lanes).await;
                let _ = reply.send(resp).await;
            }
            Request::ChunkPull { job, completed } => {
                let pull = match self.inner.parallel.get(&job) {
                    Some(j) => {
                        // A re-homing worker declares what it already
                        // finished before asking for more, so the adopting
                        // owner does not hand that work out twice (§5.4).
                        j.credit_completed(peer.node_id, &completed);
                        let name = self.member_name(&peer.node_id);
                        j.next_chunk(peer.node_id, &name)
                    }
                    // A job this node does not know is over as far as the
                    // asker is concerned; retiring is the right answer.
                    None => parallel::Pull::Done,
                };
                let resp = match pull {
                    parallel::Pull::Work(a) => Response::ChunkWork(Some(a)),
                    parallel::Pull::Wait => Response::ChunkWait,
                    parallel::Pull::Done => Response::ChunkWork(None),
                };
                let _ = reply.send(resp).await;
            }
            Request::ChunkDone(report) => {
                if let Some(job) = self.inner.parallel.get(&report.job) {
                    job.report(peer.node_id, &report);
                }
                let _ = reply.send(Response::Ok).await;
            }
            // -- ephemeral objects (M9 §5.5) ---------------------------------
            //
            // This node is one of the rendezvous-ranked directory nodes for
            // these object ids. It is not a service and it did not volunteer:
            // the sender computed the same ranking and arrived here.
            Request::ObjectAnnounce { job, objects } => {
                for (object, size) in objects {
                    // The holder is whoever TLS proved is on this
                    // connection — never a node id claimed in the payload.
                    self.inner.objdir.record(object, job, size, peer.node_id);
                }
                let _ = reply.send(Response::Ok).await;
            }
            Request::ObjectLocate { object } => {
                let _ = reply
                    .send(Response::ObjectHolders {
                        holders: self.inner.objdir.holders(&object),
                    })
                    .await;
            }
            Request::GetObject { object } => {
                // Somebody is reading these bytes, so the promise this node
                // made about them is renewed (ENG-19). This is the only
                // signal a producer ever gets that a cache entry it
                // published is still worth anything: the entry lives on a
                // *directory* node, chosen by rendezvous hashing, and that
                // node evicts it without being able to say so — there is no
                // `ObjectRelease` on the wire and there should not be one.
                // A lease that is renewed on use keeps the bytes behind
                // entries people still hit and lets the rest lapse.
                self.inner
                    .objects
                    .renew_pins(&[object], self.inner.config.cache.pin_lease);
                let _ = reply
                    .send(Response::Object(
                        self.inner.objects.manifest(&object).map(Box::new),
                    ))
                    .await;
            }
            // -- action cache (C7 §11.2) -------------------------------------
            //
            // Same shape as the object directory above it, and for the same
            // reason: this node did not volunteer and is not a service. The
            // asker computed the rendezvous ranking over the action key and
            // arrived here.
            Request::CanReach { paths, tools } => {
                // A `stat`, and nothing else (R4 P2). No read, no walk, no
                // recursion — the question is "is this mounted here", and
                // the cheapest thing that answers it is the right thing,
                // because a scheduler asks it of every candidate before it
                // places anything.
                //
                // A relative path always answers false: it would be
                // resolved against this daemon's working directory, which
                // is not the job's, so any answer would be about the wrong
                // path.
                const MAX_PATHS: usize = 32;
                let ok: Vec<bool> = paths
                    .iter()
                    .take(MAX_PATHS)
                    .map(|p| {
                        let path = std::path::Path::new(p);
                        path.is_absolute() && std::fs::metadata(path).is_ok()
                    })
                    .collect();
                // A tool check is `which`, and nothing more: the question
                // is "would this command resolve here", not "does it work".
                // Running `--version` on a stranger's word is a different
                // and much worse idea.
                let tools_ok: Vec<bool> = tools
                    .iter()
                    .take(MAX_PATHS)
                    .map(|t| which_on_path(t))
                    .collect();
                let _ = reply.send(Response::Reachable { ok, tools_ok }).await;
            }
            // Phase B, E3: what this node's instances are using, now. Only
            // jobs it actually runs are measured — a job id this node does
            // not run gets no row, not a zero — and the ask is bounded.
            Request::InstanceStats { jobs } => {
                const MAX_JOBS: usize = 64;
                let mine: Vec<JobId> = jobs
                    .into_iter()
                    .filter(|job| self.inner.jobs.is_running(job))
                    .take(MAX_JOBS)
                    .collect();
                let stats = self.measure_local_instances(mine).await;
                let _ = reply.send(Response::InstanceStats(stats)).await;
            }
            Request::CacheLookup { key } => {
                let _ = reply
                    .send(Response::CacheEntry(
                        self.inner.actions.get(&key).map(Box::new),
                    ))
                    .await;
            }
            // A bare claim, from a pre-1.33 node. A member's word was taken
            // as sufficient, which let any node at all index
            // outputs for work it never ran, for other people's runs to
            // reuse (SEC-18). Refused; a miss is free.
            Request::CachePublish { .. } => {
                let _ = reply
                    .send(Response::Error {
                        message: "a cache entry has to come with the work it was computed from \
                                  (CachePublishV2)"
                            .into(),
                    })
                    .await;
            }
            Request::CachePublishV2 { entry, proof } => {
                // The producer is whoever TLS proved is on this connection,
                // never a node id claimed in the payload — a node cannot
                // publish an entry saying somebody else computed it, for
                // the same reason it cannot announce somebody else's
                // object.
                let mut entry = *entry;
                entry.producer = peer.node_id;
                let resp = match self
                    .verify_cache_publish(peer.node_id, &entry, *proof)
                    .await
                {
                    Ok(service) => {
                        let released = self.inner.actions.put_and_evict(entry, service);
                        // Only ever unpins objects this node holds; a
                        // directory node that indexes an entry rarely holds
                        // its bytes, and `unpin` on an id it does not have
                        // is a no-op.
                        self.inner.objects.unpin(&released);
                        Response::Ok
                    }
                    Err(CacheRefusal::PlanNeeded) => Response::CachePlanNeeded,
                    Err(CacheRefusal::Refused(message)) => {
                        debug!(
                            peer = %peer.node_id.short(),
                            key = %entry.key.short(),
                            "cache entry refused: {message}"
                        );
                        Response::Error { message }
                    }
                };
                let _ = reply.send(resp).await;
            }
        }
    }
}
