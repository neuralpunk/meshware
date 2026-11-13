//! The lease table: soft reservations a node holds on behalf of a gang
//! decision that has not been made yet (C0–C9 §8.2, C4 P1).
//!
//! Before this existed, nothing in the tree could hold a resource for a
//! decision still in flight. `admits()` was local and per-task, so two
//! large submissions could each acquire half the mesh and neither finish —
//! a liveness bug, not only a missing feature.
//!
//! A lease fixes that with three properties, and every one of them is load
//! bearing:
//!
//! - **It counts.** A lease is added to what `Sched::reserved()` reports,
//!   so two concurrent gangs cannot double-book the same cores. That single
//!   sentence is the whole of all-or-nothing admission; everything else in
//!   this module is about what happens when it goes wrong.
//! - **It expires on its own.** The TTL *is* the failure model. If the
//!   origin dies between offer and commit, every lease it was holding
//!   returns without anybody noticing the death, which is why no
//!   distributed cleanup path exists in this design: there is nothing to
//!   clean up.
//! - **It yields to seniority.** A node holding a lease for a junior gang
//!   gives it up when a senior gang's offer would otherwise be refused
//!   (wound-wait). Pure retry-with-backoff livelocks exactly when it
//!   matters most — two large gangs, each holding half the mesh, each
//!   releasing and re-acquiring forever — and wound-wait cannot, because
//!   the senior gang always makes progress and seniority is monotonic in
//!   submit time so nobody starves.
//!
//! **The device ledger is this table's second kind of entry** (C5 §9.2).
//! A device is reserved by exactly the same mechanism and under exactly the
//! same lock, because the alternative — two ledgers, two locks, two answers
//! to "is device 3 free" — is how a node hands the same GPU to two jobs.
//! What differs is only the holder: a *gang* holds devices for a decision
//! not yet made, and expires; a *job* holds them while it runs, and is
//! released when it exits, however it exits.
//!
//! **Leases are never replicated.** Not an omission: replicating them would
//! create the one thing this design does not have, a distributed cleanup
//! path for an origin that died holding reservations. What replicates is
//! the *plan* (`OpBody::GangPlan`), which is enough for a successor to ask
//! the only question it has — did this gang ever commit — and to re-drive
//! the offer phase if it did not.

use mesh_proto::{
    Device, DeviceRequest, GangId, GangTicket, JobId, LeaseId, LeaseView, Requirements,
};

/// A lease that has been committed keeps its resources past the offer TTL,
/// for this long, while the member job travels from the origin to here.
///
/// Generous relative to a submission (a job spec plus a bundle check is
/// well under a second on a LAN) because the cost of being wrong in each
/// direction is asymmetric: too short and a committed gang can lose a
/// member's reservation to a racing submission, which is the exact failure
/// all-or-nothing admission exists to prevent; too long and one node holds
/// some cores idle for half a minute.
pub const COMMIT_GRACE_SECS: u64 = 30;

/// What a lease is for right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    /// The offer was accepted and the gang has not committed. Expires on
    /// the TTL, and may be **wounded** by a senior gang.
    Offered,
    /// The gang committed. It is waiting for its member job to arrive, and
    /// from here it is no longer wound-able: wounding a committed lease
    /// would tear down a gang that has already started elsewhere, which
    /// converts a scheduling delay into lost work.
    Committed,
}

/// One soft reservation.
///
/// `reqs` is what **one** member needs and `members` is how many of them
/// this node is holding — more than one under `pack`, which is the default.
/// Keeping the per-member figure rather than a pre-multiplied total is what
/// lets a member job discharge exactly its own share when it arrives, so a
/// two-member lease does not evaporate when the first of the two lands.
#[derive(Debug, Clone)]
pub struct Lease {
    pub lease: LeaseId,
    pub gang: GangId,
    pub ticket: GangTicket,
    pub reqs: Requirements,
    pub members: u32,
    pub state: LeaseState,
    pub expires_at_micros: u64,
}

impl Lease {
    /// Everything this lease is holding: one member's requirements, times
    /// the members still to arrive.
    pub fn total(&self) -> Requirements {
        Requirements {
            cpus: self.reqs.cpus.saturating_mul(self.members),
            mem_mb: self.reqs.mem_mb.saturating_mul(u64::from(self.members)),
            disk_mb: self.reqs.disk_mb.saturating_mul(u64::from(self.members)),
        }
    }

    pub fn view(&self) -> LeaseView {
        let total = self.total();
        LeaseView {
            lease: self.lease,
            gang: self.gang,
            ticket: self.ticket,
            cpus: total.cpus,
            mem_mb: total.mem_mb,
            expires_at_micros: self.expires_at_micros,
        }
    }
}

// ---------------------------------------------------------------------------
// The second kind of entry: devices (C5 P4)
// ---------------------------------------------------------------------------

/// Who is holding a node-local exclusive resource.
///
/// Two kinds, and the split is the same one the lease table already makes:
/// a gang holds on behalf of a decision that has not been made yet and
/// therefore *expires*, while a job holds because it is running and is
/// therefore released by its own exit. Nothing else may hold one — there is
/// no operator reservation, no manual pin, no third case to keep
/// consistent.
///
/// Named `Holder` rather than `DeviceHolder` since N2: ports are the second
/// resource with exactly these two kinds of claimant, and a
/// `PortHold { holder: DeviceHolder }` would have read as a mistake in a
/// place where the reader most needs to trust what they are reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// Reserved by a gang offer, and released with that lease — including
    /// when the lease expires, which is what stops a dead origin from
    /// parking a GPU farm.
    Gang(GangId),
    /// Allocated to a running job. Released on exit, cancel or crash, and
    /// **instantly** in all three cases: the release is a table edit, not a
    /// driver call, so a `SIGKILL`ed job's device is reusable as soon as
    /// the executor notices the job is gone.
    Job(JobId),
}

/// One device, spoken for.
#[derive(Debug, Clone)]
pub struct DeviceHold {
    /// Which device, vendor included.
    ///
    /// **Not the bare ordinal.** Vendor ordinals restart at zero, so on a
    /// box with a discrete NVIDIA card and an AMD integrated GPU — an
    /// ordinary desktop — an index-keyed hold on one of them reads as a
    /// hold on the other, and a job would be handed a card somebody else
    /// is already using.
    pub key: mesh_proto::DeviceKey,
    pub holder: Holder,
    /// `--gpu-share`: the holder tolerates co-tenants here.
    ///
    /// A hold is exclusive unless the *holder* said otherwise, and a shared
    /// device only accepts further holders that also said so. Silent
    /// sharing produces OOM failures that look like application bugs in
    /// somebody else's job, so consent has to be mutual (§9.2).
    pub shared: bool,
    /// Gang holds expire with their lease; job holds never do.
    pub expires_at_micros: Option<u64>,
}

/// Every lease this node currently holds.
///
/// A `Vec`, deliberately. The set is bounded by the number of gangs being
/// negotiated across a node at one instant, which is single digits on a
/// mesh doing real work; an index would cost more to keep correct than the
/// scans it saves.
#[derive(Debug, Default)]
pub struct Leases {
    held: Vec<Lease>,
    /// The device ledger. Under the same lock as `held` and for the same
    /// reason: the check and the reservation have to be atomic with respect
    /// to ordinary admission, or two jobs get the same GPU.
    devices: Vec<DeviceHold>,
    /// The port ledger (N2 P1). Under the same lock, for the same reason
    /// one step sharper: two jobs handed the same GPU degrade, and two jobs
    /// handed the same port do not start.
    ports: Vec<PortHold>,
    /// The volume ledger (N5). Under the same lock, for the same reason
    /// again — and this one is the whole phase.
    volumes: Vec<VolumeHold>,
}

// ---------------------------------------------------------------------------
// The fourth kind of entry: volumes (N5)
// ---------------------------------------------------------------------------

/// One volume, claimed.
///
/// **This row is the claim.** Not a counter somewhere else that a
/// reconciler has to keep in step with it — the row, in the table, on the
/// node that has the volume, released by the same table edit that releases
/// a port when its job exits.
///
/// That is the entire answer to the failure N5 exists to avoid. Nomad's CSI
/// claim counter lives in Raft and the mount lives on a client, and their
/// own maintainer says the two may never be reconcilable *"because a node
/// could potentially just up and disappear on us"*. A node can. Here that
/// costs nothing: its claims were never asserted anywhere but on it, so
/// when it goes they go, and there is no counter left over to be wrong.
#[derive(Debug, Clone)]
pub struct VolumeHold {
    pub volume: String,
    pub holder: Holder,
    pub mode: mesh_proto::volumes::Mode,
    /// Gang holds expire with their lease; job holds never do.
    pub expires_at_micros: Option<u64>,
}

// ---------------------------------------------------------------------------
// The third kind of entry: ports (N2 P1)
// ---------------------------------------------------------------------------

/// One host port, spoken for.
///
/// The [`DeviceHold`] shape deliberately, because N2's instruction is to
/// model this on the device ledger rather than on cpus and memory — a port
/// is exclusive, keyed, and released by a table edit rather than a driver
/// call, which is what makes a `SIGKILL`ed job's port reusable the moment
/// its executor notices rather than after something is asked to give it
/// back.
///
/// **There is no `shared` field, and that is the one place this diverges
/// from a device on purpose.** `--gpu-share` exists because two processes
/// on one card degrade each other; two processes on one port do not
/// degrade, the second simply fails to bind. There is no consenting
/// co-tenancy to express.
#[derive(Debug, Clone)]
pub struct PortHold {
    pub port: u16,
    pub holder: Holder,
    /// The label the holder knows it by, for `MESHWARE_PORT_<label>` and
    /// for anything that has to explain who has what.
    pub label: String,
    /// Gang holds expire with their lease; job holds never do.
    pub expires_at_micros: Option<u64>,
}

/// Why an offer could not be held, in the node's own terms. Translated to
/// the wire's [`mesh_proto::GangRefusal`] by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferOutcome {
    /// Held. `wounded` names the junior gangs whose leases were yielded to
    /// make room — empty in the ordinary case, and the audit trail for
    /// wound-wait when it is not.
    Accepted {
        lease: LeaseId,
        wounded: Vec<GangId>,
    },
    /// No room, and no junior lease to take it from.
    NoRoom,
    /// No room, and the resources are held by a gang this one may not
    /// wound. Named, because "who is ahead of me" is the question an
    /// operator staring at a stuck gang actually has.
    Senior { gang: GangId },
}

/// Can this machine actually bind `port` right now?
///
/// The real check behind [`Leases::hold_ports_checked`]. Binds and drops
/// immediately — the socket is not kept, because the job is going to want
/// it and holding it would be the thing stopping them.
///
/// `0.0.0.0` deliberately, not loopback: a published container port and a
/// service listening on all interfaces both take the whole number, and a
/// check against `127.0.0.1` would call such a port free.
///
/// A failure for any reason counts as *not free*. Permission denied on a
/// privileged port, a transient error — every one of them means this node
/// should pick a different number, and guessing otherwise hands a job a
/// port it cannot use.
pub fn host_can_bind(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

impl Leases {
    /// Drop leases whose time is up. Called before every read and every
    /// decision, so an expired lease is never counted, never wounded and
    /// never reported — expiry needs no timer of its own.
    pub fn sweep(&mut self, now_micros: u64) -> usize {
        let before = self.held.len();
        self.held.retain(|l| l.expires_at_micros > now_micros);
        // Devices and ports go with the lease that reserved them. Folded in
        // here rather than left to the callers, because every path that
        // sweeps leases wants all three and a caller that forgot one would
        // leave a card — or, worse, a port — held for a gang that no longer
        // exists. A leaked port is the least forgiving of the three: the
        // next instance of the same service wants that exact number.
        self.sweep_devices(now_micros);
        self.sweep_ports(now_micros);
        self.sweep_volumes(now_micros);
        before - self.held.len()
    }

    /// What the leases have promised, excluding any held for `except` — the
    /// gang whose own member job is being admitted, whose reservation is
    /// about to become that job's.
    pub fn reserved(&self, except: Option<GangId>) -> Requirements {
        self.held
            .iter()
            .filter(|l| Some(l.gang) != except)
            .map(Lease::total)
            .fold(Requirements::default(), |acc, reqs| Requirements {
                cpus: acc.cpus.saturating_add(reqs.cpus),
                mem_mb: acc.mem_mb.saturating_add(reqs.mem_mb),
                disk_mb: acc.disk_mb.saturating_add(reqs.disk_mb),
            })
    }

    /// Slots these leases are standing in for: one per member still to
    /// arrive.
    ///
    /// A lease has to occupy a *slot* as well as cores, or the two
    /// admission gates disagree: a node could grant a reservation it has
    /// the hardware for and then have no slot free to start the member in,
    /// which would queue one member of an all-at-once gang behind ordinary
    /// work. Counting it here is what makes the offer answer the same
    /// question the commit will.
    pub fn slots(&self) -> usize {
        self.held
            .iter()
            .map(|l| l.members as usize)
            .fold(0usize, usize::saturating_add)
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Lease> {
        self.held.iter()
    }

    pub fn views(&self) -> Vec<LeaseView> {
        self.held.iter().map(Lease::view).collect()
    }

    pub fn get(&self, gang: &GangId) -> Option<&Lease> {
        self.held.iter().find(|l| l.gang == *gang)
    }

    /// Take a lease for `gang`, wounding junior gangs if that is what it
    /// takes and `fits` says the room is there.
    ///
    /// `fits` answers "could this node promise these requirements on top of
    /// everything already promised, given that these gangs' leases have
    /// been given back". It is a closure because the rest of the answer —
    /// running work, queued work, the advertised hardware — lives in the
    /// job scheduler, and the two have to be consulted under one lock or
    /// the check and the insert are not atomic.
    ///
    /// **Idempotent per gang.** A re-sent offer (the origin retrying after
    /// a timeout, or a successor re-driving an adopted gang) refreshes the
    /// existing lease rather than taking a second one, because taking two
    /// would double-count the same reservation and refuse the very gang
    /// that already holds it.
    #[allow(clippy::too_many_arguments)]
    pub fn offer(
        &mut self,
        gang: GangId,
        ticket: GangTicket,
        reqs: Requirements,
        members: u32,
        expires_at_micros: u64,
        now_micros: u64,
        fits: impl Fn(&Requirements, u32, &[GangId]) -> bool,
    ) -> OfferOutcome {
        self.sweep(now_micros);
        let members = members.max(1);

        if let Some(existing) = self.held.iter_mut().find(|l| l.gang == gang) {
            // Re-offer: same reservation, fresh clock. Never a downgrade —
            // a committed lease that is re-offered stays committed, so a
            // duplicate offer racing a commit cannot un-commit it.
            existing.reqs = reqs;
            existing.members = members;
            existing.ticket = ticket;
            existing.expires_at_micros = existing.expires_at_micros.max(expires_at_micros);
            return OfferOutcome::Accepted {
                lease: existing.lease,
                wounded: Vec::new(),
            };
        }

        if fits(&reqs, members, &[]) {
            return self.insert(gang, ticket, reqs, members, expires_at_micros, Vec::new());
        }

        // Wound-wait. Junior *and* uncommitted, most junior first: yielding
        // the most junior lease that makes room disturbs the least work,
        // and a committed lease is a gang that has already started
        // elsewhere — taking it back would convert a scheduling delay into
        // lost work.
        let mut juniors: Vec<(GangTicket, GangId)> = self
            .held
            .iter()
            .filter(|l| l.state == LeaseState::Offered && ticket.is_senior_to(&l.ticket))
            .map(|l| (l.ticket, l.gang))
            .collect();
        juniors.sort_by_key(|a| std::cmp::Reverse(a.0));

        let mut yielding: Vec<GangId> = Vec::new();
        for (_, junior) in juniors {
            yielding.push(junior);
            if fits(&reqs, members, &yielding) {
                self.held.retain(|l| !yielding.contains(&l.gang));
                return self.insert(gang, ticket, reqs, members, expires_at_micros, yielding);
            }
        }

        // Nothing that could be yielded would have made room. If another
        // gang is holding these resources, name it — that is a different
        // answer from "the mesh is too small", and it is the difference
        // between waiting and resizing.
        //
        // The named gang is the most senior holder, which in the ordinary
        // case is a gang genuinely ahead in the order, and in one other
        // case is a *junior* gang that has already committed. Both are
        // "held by a gang this one may not wound", and reporting the
        // committed one as "no room" would be true about capacity and
        // useless about why.
        match self.held.iter().min_by_key(|l| l.ticket) {
            Some(holder) => OfferOutcome::Senior { gang: holder.gang },
            None => OfferOutcome::NoRoom,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        gang: GangId,
        ticket: GangTicket,
        reqs: Requirements,
        members: u32,
        expires_at_micros: u64,
        wounded: Vec<GangId>,
    ) -> OfferOutcome {
        let lease = LeaseId::generate();
        self.held.push(Lease {
            lease,
            gang,
            ticket,
            reqs,
            members,
            state: LeaseState::Offered,
            expires_at_micros,
        });
        OfferOutcome::Accepted { lease, wounded }
    }

    /// One member of a committed gang has arrived: redeem its share of the
    /// reservation, and drop the lease once the last member is in.
    ///
    /// `true` means the job was admitted **by the lease** and needs no
    /// capacity check of its own — the check happened at offer time, and
    /// re-running it here would let ordinary work that arrived in between
    /// refuse a gang member the mesh had already promised.
    pub fn discharge(&mut self, gang: &GangId, now_micros: u64) -> bool {
        self.sweep(now_micros);
        let Some(idx) = self.held.iter().position(|l| l.gang == *gang) else {
            return false;
        };
        let lease = &mut self.held[idx];
        lease.members = lease.members.saturating_sub(1);
        if lease.members == 0 {
            self.held.remove(idx);
        }
        true
    }

    /// Turn an offered lease into a committed one. `None` when the lease is
    /// gone — expired or wounded — which is exactly the case the origin has
    /// to hear about, because it means the gang did not get all `N` and
    /// must release the rest rather than start `N-1`.
    pub fn commit(&mut self, gang: &GangId, now_micros: u64) -> Option<LeaseId> {
        self.sweep(now_micros);
        let lease = self.held.iter_mut().find(|l| l.gang == *gang)?;
        lease.state = LeaseState::Committed;
        lease.expires_at_micros = now_micros.saturating_add(COMMIT_GRACE_SECS * 1_000_000);
        Some(lease.lease)
    }

    /// Give a lease back. Idempotent: releasing one this node does not hold
    /// is a no-op, which is what lets an adopting successor release
    /// blindly before it re-drives.
    ///
    /// Takes the gang's device holds with it. A lease that gave its cores
    /// back and kept its GPUs would be the worst of both — the node would
    /// look free to a bin-packer and refuse every device request.
    pub fn release(&mut self, gang: &GangId) -> bool {
        let before = self.held.len();
        self.held.retain(|l| l.gang != *gang);
        self.release_devices(&Holder::Gang(*gang));
        self.release_ports(&Holder::Gang(*gang));
        self.release_volumes(&Holder::Gang(*gang));
        before != self.held.len()
    }

    // -- the device ledger (C5 P4) ------------------------------------------

    /// Drop device holds whose gang lease has expired.
    ///
    /// Job holds are untouched: a job's hold ends when the job does, and a
    /// long training run is not a stale reservation.
    pub fn sweep_devices(&mut self, now_micros: u64) {
        self.devices
            .retain(|d| d.expires_at_micros.is_none_or(|at| at > now_micros));
    }

    /// Every device hold, expired ones already swept.
    pub fn device_holds(&self) -> &[DeviceHold] {
        &self.devices
    }

    /// Drop port holds whose gang lease has expired (N2 P1).
    ///
    /// Job holds are untouched, for the reason `sweep_devices` gives: a
    /// job's hold ends when the job does, and a service that is supposed to
    /// stay up is not a stale reservation.
    pub fn sweep_ports(&mut self, now_micros: u64) {
        self.ports
            .retain(|p| p.expires_at_micros.is_none_or(|at| at > now_micros));
    }

    /// Which devices from `inventory` could satisfy `req` right now, best
    /// first.
    ///
    /// "Best" is *most capable among the free ones*, then lowest index — a
    /// deterministic order, so two nodes asked the same question at the
    /// same instant give the same answer and a packed gang's members do not
    /// each pick device 0.
    ///
    /// **Capacity breaks the tie before the ordinal does**, because an
    /// ordinal is an arbitrary label and memory is not. A desktop with a
    /// discrete card and an integrated GPU has two devices that both call
    /// themselves index 0, and ordering them by anything derived from the
    /// name — vendor, ordinal — hands a CUDA job the 512MB display adapter
    /// roughly half the time. A request that named no VRAM floor wants the
    /// best card free, not the alphabetically first one.
    ///
    /// `busy_of` reports how loaded a device is from live telemetry (P5),
    /// in `0.0..=1.0`. When it is known it outranks the index, because the
    /// whole point of the term is that five GPU jobs should distribute by
    /// real idleness rather than by ordinal.
    pub fn free_devices<'a>(
        &self,
        inventory: &'a [Device],
        req: &DeviceRequest,
        now_micros: u64,
        busy_of: impl Fn(&Device) -> Option<f64>,
    ) -> Vec<&'a Device> {
        let mut candidates: Vec<(u8, u64, std::cmp::Reverse<u64>, u32, &Device)> = inventory
            .iter()
            .filter(|d| d.matches(req))
            .filter_map(|d| {
                // Expired holds are ignored here rather than relying on a
                // sweep having happened first: this is the read every
                // admission and promotion goes through, and a stale hold
                // that survived one missed sweep would idle a card until
                // somebody noticed.
                let key = d.key();
                let holders: Vec<&DeviceHold> = self
                    .devices
                    .iter()
                    .filter(|h| h.key == key)
                    .filter(|h| h.expires_at_micros.is_none_or(|at| at > now_micros))
                    .collect();
                // Free, or shared-by-consent: both this request and every
                // current holder must have opted in.
                let usable = holders.is_empty() || (req.share && holders.iter().all(|h| h.shared));
                if !usable {
                    return None;
                }
                // Prefer an untouched device over a shareable one that is
                // already in use — sharing is a fallback, never a first
                // choice, even when the job consented to it.
                let tenancy = u8::try_from(holders.len()).unwrap_or(u8::MAX);
                // Quantised so that a telemetry wobble of a fraction of a
                // percent does not reorder devices between two members of
                // the same gang.
                let busy = busy_of(d)
                    .map(|b| (b.clamp(0.0, 1.0) * 10.0).round() as u64)
                    .unwrap_or(5);
                Some((tenancy, busy, std::cmp::Reverse(d.mem_mb), d.index, d))
            })
            .collect();
        candidates.sort_by(|a, b| {
            (a.0, a.1, a.2, a.3, &a.4.vendor).cmp(&(b.0, b.1, b.2, b.3, &b.4.vendor))
        });
        candidates.into_iter().map(|(_, _, _, _, d)| d).collect()
    }

    /// Take `devices` for `holder`. Idempotent per holder: re-holding what
    /// this holder already has refreshes rather than double-books.
    pub fn hold_devices(
        &mut self,
        holder: Holder,
        devices: &[&Device],
        shared: bool,
        expires_at_micros: Option<u64>,
    ) {
        self.release_devices(&holder);
        for d in devices {
            self.devices.push(DeviceHold {
                key: d.key(),
                holder,
                shared,
                expires_at_micros,
            });
        }
    }

    /// Give a holder's devices back. Idempotent.
    pub fn release_devices(&mut self, holder: &Holder) -> usize {
        let before = self.devices.len();
        self.devices.retain(|d| d.holder != *holder);
        before - self.devices.len()
    }

    /// What one holder is holding.
    ///
    /// Full keys, not bare ordinals. Handing back an index alone would make
    /// the caller look the device up again by a number that does not
    /// identify one — which is how a job leased a discrete card and was
    /// then handed the integrated GPU that shares its ordinal.
    pub fn devices_of(&self, holder: &Holder) -> Vec<mesh_proto::DeviceKey> {
        self.devices
            .iter()
            .filter(|d| d.holder == *holder)
            .map(|d| d.key.clone())
            .collect()
    }

    /// Is this host port free for `holder` to take?
    ///
    /// Free means *nobody else* holds it: re-asking about a port this
    /// holder already has answers true, which is what makes
    /// [`Self::hold_ports`] idempotent rather than self-blocking on a
    /// retry.
    pub fn port_free(&self, port: u16, holder: &Holder) -> bool {
        !self
            .ports
            .iter()
            .any(|p| p.port == port && p.holder != *holder)
    }

    /// Allocate the ports a job asked for, or refuse the whole request.
    ///
    /// **All or nothing.** A job that asked for `http` and `metrics` and
    /// was given one of them starts, binds half of what it needs, and fails
    /// in its own code with a message about its own config — so a partial
    /// allocation is worse than a refusal, and the caller gets `None` to
    /// spill to another node with.
    ///
    /// Static ports are taken as asked; dynamic ones are drawn from
    /// `range`, skipping anything held and anything reserved. Idempotent
    /// per holder: re-allocating for a holder that already has ports
    /// releases them first, so a retried admission does not double-book.
    pub fn hold_ports(
        &mut self,
        holder: Holder,
        requests: &[mesh_proto::PortRequest],
        range: &mesh_proto::PortRange,
        reserved: &[u16],
        expires_at_micros: Option<u64>,
    ) -> Option<Vec<mesh_proto::PortAllocation>> {
        // Table-only: every caller that can reach the network should use
        // `hold_ports_checked` instead.
        self.hold_ports_checked(
            holder,
            requests,
            range,
            reserved,
            expires_at_micros,
            &|_| true,
        )
    }

    /// [`Self::hold_ports`], asking the machine whether the port is really
    /// free before handing it out.
    ///
    /// **The ledger is not the only thing on the host.** It knows what
    /// *this node* has lent, and nothing about a second meshware node on
    /// the same machine, a database somebody installed, or a container
    /// orphaned by a crash. A number that is free in the table and taken on
    /// the box is handed to a job that then dies on `EADDRINUSE`, restarts,
    /// is handed the same number, and crash-loops — while `meshnode ports`
    /// insists the port is free, because the table it prints is the table
    /// that was wrong.
    ///
    /// `is_bindable` is injected rather than called directly so the lease
    /// table stays a data structure with no syscalls in it: tests drive it
    /// with a closure, the daemon passes a real bind.
    ///
    /// **The race is real and bounded.** Between the check and the job
    /// binding, something outside the mesh could take the port; nothing
    /// short of holding the socket open and passing the fd can close that,
    /// and the lease already prevents the case that actually happens — two
    /// jobs on this node. This turns a permanent crash loop into a rare
    /// retry.
    pub fn hold_ports_checked(
        &mut self,
        holder: Holder,
        requests: &[mesh_proto::PortRequest],
        range: &mesh_proto::PortRange,
        reserved: &[u16],
        expires_at_micros: Option<u64>,
        is_bindable: &dyn Fn(u16) -> bool,
    ) -> Option<Vec<mesh_proto::PortAllocation>> {
        self.release_ports(&holder);
        let mut taken: Vec<u16> = Vec::with_capacity(requests.len());
        let mut out = Vec::with_capacity(requests.len());

        // Static first, always. A dynamic draw that happened to take the
        // number a later static request demands would turn one job's
        // request into a refusal of itself.
        let mut ordered: Vec<&mesh_proto::PortRequest> = requests.iter().collect();
        ordered.sort_by_key(|r| !r.is_static());

        for req in ordered {
            let port = match req.fixed {
                Some(fixed) => {
                    // A demanded port that the host already has is refused
                    // here rather than discovered as a bind failure — the
                    // job spills to a node that can give it, which is what
                    // the operator meant by demanding it.
                    if reserved.contains(&fixed)
                        || taken.contains(&fixed)
                        || !self.port_free(fixed, &holder)
                        || !is_bindable(fixed)
                    {
                        return None;
                    }
                    fixed
                }
                None => {
                    // A linear sweep of a 12,000-port range, skipping what
                    // is held. Bounded by the range and cheap at the
                    // occupancies a node reaches — a node with twelve
                    // thousand live ports has a different problem.
                    let mut found = None;
                    for candidate in range.min..=range.max {
                        if reserved.contains(&candidate)
                            || taken.contains(&candidate)
                            || !self.port_free(candidate, &holder)
                        {
                            continue;
                        }
                        // Cheap in the ordinary case: the sweep stops at
                        // the first candidate free in the table, and on a
                        // node that is not fighting anything for ports that
                        // is also the first one free on the host.
                        if !is_bindable(candidate) {
                            continue;
                        }
                        found = Some(candidate);
                        break;
                    }
                    found?
                }
            };
            taken.push(port);
            out.push(mesh_proto::PortAllocation {
                label: req.label.clone(),
                host: port,
                // Host mode: there is no translation, so the port inside
                // the job *is* the host port. `to` is kept as the
                // operator's declared intent for the registry to read.
                to: port,
            });
        }

        for alloc in &out {
            self.ports.push(PortHold {
                port: alloc.host,
                holder,
                label: alloc.label.clone(),
                expires_at_micros,
            });
        }
        Some(out)
    }

    /// Give a holder's ports back. Idempotent.
    pub fn release_ports(&mut self, holder: &Holder) -> usize {
        let before = self.ports.len();
        self.ports.retain(|p| p.holder != *holder);
        before - self.ports.len()
    }

    /// What one holder is holding.
    pub fn ports_of(&self, holder: &Holder) -> Vec<mesh_proto::PortAllocation> {
        self.ports
            .iter()
            .filter(|p| p.holder == *holder)
            .map(|p| mesh_proto::PortAllocation {
                label: p.label.clone(),
                host: p.port,
                to: p.port,
            })
            .collect()
    }

    /// The ledger as a view: every hold, with who has it and which kind of
    /// wait an operator is in (N2 P1).
    pub fn port_ledger(&self) -> Vec<mesh_proto::PortHeldView> {
        let mut out: Vec<mesh_proto::PortHeldView> = self
            .ports
            .iter()
            .map(|p| mesh_proto::PortHeldView {
                port: p.port,
                label: p.label.clone(),
                holder: match p.holder {
                    Holder::Job(id) => id.short(),
                    Holder::Gang(id) => id.short(),
                },
                kind: match p.holder {
                    Holder::Job(_) => "job".into(),
                    Holder::Gang(_) => "gang".into(),
                },
            })
            .collect();
        out.sort_by_key(|h| h.port);
        out
    }

    /// Every port held on this node right now, for `top` and `doctor`.
    pub fn ports_held(&self) -> Vec<(u16, String)> {
        let mut out: Vec<(u16, String)> = self
            .ports
            .iter()
            .map(|p| (p.port, p.label.clone()))
            .collect();
        out.sort_unstable();
        out
    }

    /// May `holder` claim `volume` in `mode` right now?
    ///
    /// Read-only claims share freely; a write claim is exclusive against
    /// everything, readers included. A holder re-asking about what it
    /// already has answers true, which is what makes
    /// [`Self::hold_volumes`] idempotent on a retry rather than
    /// self-blocking.
    pub fn volume_free(
        &self,
        volume: &str,
        holder: &Holder,
        mode: mesh_proto::volumes::Mode,
    ) -> bool {
        !self.volumes.iter().any(|v| {
            v.volume == volume
                && v.holder != *holder
                // Two readers coexist. Everything else collides.
                && !(v.mode.read_only() && mode.read_only())
        })
    }

    /// Claim the volumes a job asked for, or refuse the whole request.
    ///
    /// **All or nothing**, for the reason ports are: a job that asked for
    /// its data directory and its cache and got one of them starts, writes
    /// half its state somewhere it did not mean to, and fails in its own
    /// code about its own paths.
    ///
    /// Idempotent per holder — re-claiming releases first — so a retried
    /// admission cannot double-book against itself.
    pub fn hold_volumes(
        &mut self,
        holder: Holder,
        requests: &[mesh_proto::VolumeRequest],
        available: &[mesh_proto::Volume],
        expires_at_micros: Option<u64>,
    ) -> Option<Vec<mesh_proto::Volume>> {
        self.release_volumes(&holder);
        let mut granted = Vec::with_capacity(requests.len());
        for req in requests {
            let vol = available.iter().find(|v| v.name == req.name)?;
            // A node that declared a volume read-only refuses a write claim
            // whatever the job asked for: the operator's statement about
            // the directory outranks the job's wish for it.
            if vol.read_only && !req.mode.read_only() {
                return None;
            }
            if !self.volume_free(&req.name, &holder, req.mode) {
                return None;
            }
            // Against this job's own earlier claims in the same request: a
            // job asking for one volume twice, once to write, is a
            // contradiction rather than a grant.
            if granted
                .iter()
                .any(|g: &mesh_proto::Volume| g.name == req.name)
            {
                return None;
            }
            granted.push(vol.clone());
        }
        for (req, vol) in requests.iter().zip(granted.iter()) {
            self.volumes.push(VolumeHold {
                volume: vol.name.clone(),
                holder,
                mode: req.mode,
                expires_at_micros,
            });
        }
        Some(granted)
    }

    /// Give a holder's volumes back. Idempotent.
    ///
    /// The whole of "releasing a claim". There is no counter to decrement,
    /// no second system to tell, and no `deregister --force` to need.
    pub fn release_volumes(&mut self, holder: &Holder) -> usize {
        let before = self.volumes.len();
        self.volumes.retain(|v| v.holder != *holder);
        before - self.volumes.len()
    }

    /// Drop volume claims whose gang lease has expired.
    pub fn sweep_volumes(&mut self, now_micros: u64) {
        self.volumes
            .retain(|v| v.expires_at_micros.is_none_or(|at| at > now_micros));
    }

    /// Who is holding `volume`, and whether a writer could take it.
    pub fn volume_holders(&self, volume: &str) -> (Vec<String>, bool) {
        let mut who = Vec::new();
        let mut write_held = false;
        for v in self.volumes.iter().filter(|v| v.volume == volume) {
            who.push(match v.holder {
                Holder::Job(id) => id.short(),
                Holder::Gang(id) => id.short(),
            });
            if !v.mode.read_only() {
                write_held = true;
            }
        }
        who.sort();
        (who, write_held)
    }

    /// Push a gang's device holds out to `until` — what a commit does, so
    /// the cards keep the same clock as the cores while the member job
    /// travels from the origin to here.
    pub fn extend_device_holds(&mut self, gang: &GangId, until: u64) {
        let holder = Holder::Gang(*gang);
        for hold in self.devices.iter_mut().filter(|d| d.holder == holder) {
            hold.expires_at_micros = Some(hold.expires_at_micros.unwrap_or(until).max(until));
        }
    }

    /// A gang member has arrived: move `count` of the gang's reserved
    /// devices onto the member job.
    ///
    /// **A transfer, not a new allocation**, for the same reason
    /// [`Leases::discharge`] exists: re-running the choice here would let
    /// ordinary work that arrived in between take a device the mesh had
    /// already promised the gang. The gang keeps whatever the remaining
    /// members still need, so a two-member packed lease does not lose its
    /// second GPU when the first member lands.
    pub fn claim_from_gang(&mut self, gang: &GangId, job: JobId, count: usize) -> Vec<u32> {
        let from = Holder::Gang(*gang);
        let mut moved = Vec::with_capacity(count);
        for hold in self.devices.iter_mut().filter(|d| d.holder == from) {
            if moved.len() == count {
                break;
            }
            hold.holder = Holder::Job(job);
            // A job's hold does not expire — it ends when the job does.
            hold.expires_at_micros = None;
            moved.push(hold.key.index);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::{Hlc, JobPriority};

    fn ticket(at: u64, id: u8) -> GangTicket {
        GangTicket::new(
            JobPriority::Normal,
            Hlc {
                wall_micros: at,
                logical: 0,
            },
            GangId([id; 16]),
        )
    }

    fn reqs(cpus: u32) -> Requirements {
        Requirements {
            cpus,
            mem_mb: 0,
            disk_mb: 0,
        }
    }

    /// Offer against a node with `cores` cores and nothing else on it.
    ///
    /// The closure the real caller passes reads the job scheduler's running
    /// and queued work too; here the leases are the whole story, which is
    /// what makes these tests about wound-wait rather than about admission.
    fn offer_n(
        leases: &mut Leases,
        cores: u32,
        t: GangTicket,
        cpus: u32,
        members: u32,
        now: u64,
        ttl: u64,
    ) -> OfferOutcome {
        let snapshot: Vec<(GangId, u32)> =
            leases.iter().map(|l| (l.gang, l.total().cpus)).collect();
        leases.offer(
            t.gang,
            t,
            reqs(cpus),
            members,
            now + ttl,
            now,
            |want, members, ignoring| {
                let held: u32 = snapshot
                    .iter()
                    .filter(|(g, _)| !ignoring.contains(g))
                    .map(|(_, c)| c)
                    .sum();
                held + want.cpus * members <= cores
            },
        )
    }

    fn offer(
        leases: &mut Leases,
        cores: u32,
        t: GangTicket,
        cpus: u32,
        now: u64,
        ttl: u64,
    ) -> OfferOutcome {
        offer_n(leases, cores, t, cpus, 1, now, ttl)
    }

    #[test]
    fn two_gangs_cannot_double_book_the_same_cores() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        let b = ticket(20, 2);
        assert!(matches!(
            offer(&mut l, 8, a, 8, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        // b is junior and the node is full: the whole point of the table.
        assert_eq!(
            offer(&mut l, 8, b, 8, 1_000, 10_000),
            OfferOutcome::Senior { gang: a.gang }
        );
    }

    #[test]
    fn a_lease_expires_on_its_own_clock() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        assert!(matches!(
            offer(&mut l, 8, a, 8, 1_000, 5_000_000),
            OfferOutcome::Accepted { .. }
        ));
        assert_eq!(l.reserved(None).cpus, 8);
        // The origin died here. Nobody is told, and nothing has to notice.
        l.sweep(1_000 + 5_000_001);
        assert!(l.is_empty(), "an expired lease returns its resources");
        assert_eq!(l.reserved(None).cpus, 0);
        assert_eq!(l.slots(), 0);
    }

    #[test]
    fn a_senior_gang_wounds_a_junior_one() {
        let mut l = Leases::default();
        let junior = ticket(200, 2);
        let senior = ticket(100, 1);
        assert!(matches!(
            offer(&mut l, 8, junior, 8, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        match offer(&mut l, 8, senior, 8, 1_000, 10_000) {
            OfferOutcome::Accepted { wounded, .. } => {
                assert_eq!(wounded, vec![junior.gang], "the junior lease is yielded");
            }
            other => panic!("the senior gang must make progress: {other:?}"),
        }
        assert_eq!(l.len(), 1);
        assert_eq!(l.get(&senior.gang).map(|x| x.reqs.cpus), Some(8));
        assert!(l.get(&junior.gang).is_none());
    }

    #[test]
    fn a_committed_lease_is_never_wounded() {
        let mut l = Leases::default();
        let junior = ticket(200, 2);
        let senior = ticket(100, 1);
        assert!(matches!(
            offer(&mut l, 8, junior, 8, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        // The junior gang got all its acceptances first and committed. It
        // is a running gang now; taking its reservation back would kill
        // work rather than delay it.
        assert!(l.commit(&junior.gang, 1_100).is_some());
        assert_eq!(
            offer(&mut l, 8, senior, 8, 1_200, 10_000),
            OfferOutcome::Senior { gang: junior.gang },
            "a committed lease outranks seniority"
        );
    }

    #[test]
    fn wounding_takes_the_most_junior_lease_that_makes_room() {
        let mut l = Leases::default();
        let mid = ticket(200, 2);
        let most_junior = ticket(300, 3);
        let senior = ticket(100, 1);
        assert!(matches!(
            offer(&mut l, 8, mid, 4, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        assert!(matches!(
            offer(&mut l, 8, most_junior, 4, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        match offer(&mut l, 8, senior, 4, 1_000, 10_000) {
            OfferOutcome::Accepted { wounded, .. } => {
                assert_eq!(
                    wounded,
                    vec![most_junior.gang],
                    "disturb the least senior work that is enough"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(l.get(&mid.gang).is_some(), "the mid gang keeps its lease");
    }

    #[test]
    fn a_re_offer_refreshes_rather_than_double_counting() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        let first = match offer(&mut l, 8, a, 8, 1_000, 10_000) {
            OfferOutcome::Accepted { lease, .. } => lease,
            other => panic!("{other:?}"),
        };
        // An origin that timed out and retried, or a successor re-driving.
        // Taking a second lease would refuse the gang that already holds it.
        match offer(&mut l, 8, a, 8, 2_000, 10_000) {
            OfferOutcome::Accepted { lease, .. } => assert_eq!(lease, first),
            other => panic!("a re-offer must be idempotent: {other:?}"),
        }
        assert_eq!(l.len(), 1);
        assert_eq!(l.reserved(None).cpus, 8);
    }

    #[test]
    fn the_gangs_own_member_does_not_count_against_itself() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        assert!(matches!(
            offer(&mut l, 8, a, 8, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        assert_eq!(l.reserved(Some(a.gang)).cpus, 0);
        assert_eq!(l.reserved(None).cpus, 8);
    }

    #[test]
    fn a_packed_lease_holds_every_member_it_was_given() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        // `pack`: two of the gang's members land on this one node.
        assert!(matches!(
            offer_n(&mut l, 16, a, 4, 2, 1_000, 10_000),
            OfferOutcome::Accepted { .. }
        ));
        assert_eq!(l.reserved(None).cpus, 8, "two members, four cores each");
        assert_eq!(l.slots(), 2);

        assert!(l.commit(&a.gang, 1_100).is_some());
        // The first member arrives. The second's reservation must survive
        // it, or the gang loses half its cores between its own two jobs.
        assert!(l.discharge(&a.gang, 1_200));
        assert_eq!(l.reserved(None).cpus, 4);
        assert_eq!(l.slots(), 1);
        assert!(l.discharge(&a.gang, 1_300));
        assert!(l.is_empty());
        assert!(!l.discharge(&a.gang, 1_400), "nothing left to redeem");
    }

    #[test]
    fn release_is_idempotent() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        offer(&mut l, 8, a, 4, 1_000, 10_000);
        assert!(l.release(&a.gang));
        assert!(!l.release(&a.gang), "releasing twice is a no-op");
        assert!(!l.release(&GangId([9u8; 16])));
    }

    #[test]
    fn commit_reports_a_lease_that_is_already_gone() {
        let mut l = Leases::default();
        let a = ticket(10, 1);
        offer(&mut l, 8, a, 4, 1_000, 10_000);
        // Wounded or expired before the commit landed: the origin must hear
        // about it, because starting N-1 of N is the failure C4 prevents.
        l.release(&a.gang);
        assert!(l.commit(&a.gang, 2_000).is_none());
    }

    #[test]
    fn two_large_gangs_do_not_livelock() {
        // The C4 §14.4 property, at the level of one node's table: two
        // gangs that each want the whole node, offered over and over.
        // Under backoff this alternates forever; under wound-wait the
        // senior one takes it and keeps it.
        let mut l = Leases::default();
        let senior = ticket(100, 1);
        let junior = ticket(200, 2);
        let mut now = 1_000u64;
        for _ in 0..8 {
            offer(&mut l, 8, junior, 8, now, 10_000);
            offer(&mut l, 8, senior, 8, now, 10_000);
            now += 1_000;
        }
        assert_eq!(l.len(), 1);
        assert_eq!(
            l.get(&senior.gang).map(|x| x.gang),
            Some(senior.gang),
            "the senior gang holds the node after every round"
        );
    }
}

// ---------------------------------------------------------------------------
// N2 P1 — the port ledger
// ---------------------------------------------------------------------------

#[cfg(test)]
mod port_tests {
    use super::*;
    use mesh_proto::{JobId, PortRange, PortRequests};

    fn job(byte: u8) -> Holder {
        Holder::Job(JobId([byte; 16]))
    }

    fn small() -> PortRange {
        PortRange {
            min: 20000,
            max: 20002,
        }
    }

    #[test]
    fn a_dynamic_request_draws_from_the_range_and_holds_it() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:0,metrics:0");
        let got = l
            .hold_ports(job(1), &reqs.0, &small(), &[], None)
            .expect("two of three");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].host, 20000);
        assert_eq!(got[1].host, 20001);
        assert_eq!(
            l.ports_held(),
            vec![(20000, "http".into()), (20001, "metrics".into())]
        );
    }

    /// Two jobs must never be handed one port. Unlike a GPU, there is no
    /// consenting co-tenancy to fall back on — the second one simply does
    /// not start.
    #[test]
    fn a_second_job_never_gets_a_held_port() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:0");
        assert_eq!(
            l.hold_ports(job(1), &reqs.0, &small(), &[], None).unwrap()[0].host,
            20000
        );
        assert_eq!(
            l.hold_ports(job(2), &reqs.0, &small(), &[], None).unwrap()[0].host,
            20001,
            "the second job draws the next free port, not the same one"
        );
        assert!(!l.port_free(20000, &job(2)));
        assert!(
            l.port_free(20000, &job(1)),
            "a holder's own port is free to it"
        );
    }

    /// Half an allocation is worse than none: the job would start, bind
    /// what it got, and fail in its own code about its own config.
    #[test]
    fn a_partial_allocation_is_refused_whole() {
        let mut l = Leases::default();
        // Fill the three-port range.
        l.hold_ports(
            job(1),
            &PortRequests::parse("a:0,b:0,c:0").0,
            &small(),
            &[],
            None,
        )
        .expect("fills the range");
        // A two-port job now fits nowhere, and gets nothing rather than one.
        assert!(
            l.hold_ports(
                job(2),
                &PortRequests::parse("x:0,y:0").0,
                &small(),
                &[],
                None
            )
            .is_none()
        );
        assert_eq!(l.ports_of(&job(2)).len(), 0, "and holds nothing");
        assert_eq!(l.ports_held().len(), 3, "the first job keeps all three");
    }

    /// A static port is taken as asked, and refused outright when it is
    /// spoken for — never quietly substituted, because the number *is* the
    /// request.
    #[test]
    fn a_static_port_is_exact_or_refused() {
        let mut l = Leases::default();
        let fixed = PortRequests::parse("http:8080!");
        assert_eq!(
            l.hold_ports(job(1), &fixed.0, &small(), &[], None).unwrap()[0].host,
            8080,
            "outside the dynamic range, because it was demanded"
        );
        assert!(
            l.hold_ports(job(2), &fixed.0, &small(), &[], None)
                .is_none(),
            "the second asker is refused rather than given 8081"
        );
    }

    /// Static before dynamic, or a dynamic draw could take the number a
    /// later static request in the *same job* demands — turning one job's
    /// request into a refusal of itself.
    #[test]
    fn a_static_port_is_taken_before_the_dynamic_draw() {
        let mut l = Leases::default();
        // `low` would be drawn 20000; `fixed` demands 20000 too.
        let reqs = PortRequests::parse("low:0,fixed:20000!");
        let got = l
            .hold_ports(job(1), &reqs.0, &small(), &[], None)
            .expect("both, in some order");
        let fixed = got.iter().find(|a| a.label == "fixed").unwrap();
        let low = got.iter().find(|a| a.label == "low").unwrap();
        assert_eq!(fixed.host, 20000);
        assert_ne!(low.host, 20000);
    }

    #[test]
    fn a_reserved_port_is_never_handed_out() {
        let mut l = Leases::default();
        let got = l
            .hold_ports(
                job(1),
                &PortRequests::parse("http:0").0,
                &small(),
                &[20000],
                None,
            )
            .unwrap();
        assert_eq!(got[0].host, 20001, "20000 is reserved");
        assert!(
            l.hold_ports(
                job(2),
                &PortRequests::parse("http:20000!").0,
                &small(),
                &[20000],
                None
            )
            .is_none(),
            "and cannot be demanded either"
        );
    }

    /// The property the whole ledger is built around: a killed job's port
    /// is reusable the instant its executor notices, because the release is
    /// a table edit. The next instance of the same service wants that exact
    /// number.
    #[test]
    fn a_released_port_is_immediately_reusable() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:20000!");
        assert!(l.hold_ports(job(1), &reqs.0, &small(), &[], None).is_some());
        assert_eq!(l.release_ports(&job(1)), 1);
        assert!(
            l.hold_ports(job(2), &reqs.0, &small(), &[], None).is_some(),
            "the replacement instance gets the same number back"
        );
    }

    /// Re-allocating for a holder that already has ports releases them
    /// first, so a retried admission does not double-book against itself.
    #[test]
    fn re_holding_is_idempotent_per_holder() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:0");
        let first = l.hold_ports(job(1), &reqs.0, &small(), &[], None).unwrap();
        let again = l.hold_ports(job(1), &reqs.0, &small(), &[], None).unwrap();
        assert_eq!(first[0].host, again[0].host);
        assert_eq!(l.ports_held().len(), 1, "not two rows for one job");
    }

    /// A gang's ports expire with its lease; a job's never do. A leaked
    /// port is the least forgiving of the three ledgers, because the next
    /// instance of the same service wants that exact number.
    #[test]
    fn a_gang_port_expires_with_its_lease_and_a_job_port_does_not() {
        let mut l = Leases::default();
        let gang = Holder::Gang(mesh_proto::GangId([9; 16]));
        let reqs = PortRequests::parse("http:0");
        l.hold_ports(gang, &reqs.0, &small(), &[], Some(1_000))
            .expect("held");
        l.hold_ports(
            job(1),
            &PortRequests::parse("other:0").0,
            &small(),
            &[],
            None,
        )
        .expect("held");
        assert_eq!(l.ports_held().len(), 2);
        l.sweep_ports(2_000);
        assert_eq!(l.ports_held().len(), 1, "the gang's went, the job's stayed");
        assert_eq!(l.ports_of(&job(1)).len(), 1);
    }
}

// ---------------------------------------------------------------------------
// N5 — the volume ledger, and the bug class it closes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod volume_tests {
    use super::*;
    use mesh_proto::volumes::Mode;
    use mesh_proto::{JobId, Volume, VolumeRequest};

    fn job(byte: u8) -> Holder {
        Holder::Job(JobId([byte; 16]))
    }

    fn vol(name: &str, read_only: bool) -> Volume {
        Volume {
            name: name.into(),
            path: format!("/srv/{name}"),
            read_only,
            source: "static".into(),
        }
    }

    fn want(name: &str, mode: Mode) -> VolumeRequest {
        VolumeRequest {
            name: name.into(),
            path: format!("/{name}"),
            mode,
        }
    }

    fn available() -> Vec<Volume> {
        vec![vol("data", false), vol("cache", false), vol("ref", true)]
    }

    #[test]
    fn a_write_claim_is_exclusive() {
        let mut l = Leases::default();
        assert!(
            l.hold_volumes(job(1), &[want("data", Mode::ReadWrite)], &available(), None)
                .is_some()
        );
        assert!(
            l.hold_volumes(job(2), &[want("data", Mode::ReadWrite)], &available(), None)
                .is_none(),
            "a second writer is refused"
        );
        assert!(
            l.hold_volumes(job(2), &[want("data", Mode::ReadOnly)], &available(), None)
                .is_none(),
            "and so is a reader, while a writer holds it"
        );
    }

    #[test]
    fn readers_share_freely() {
        let mut l = Leases::default();
        for n in 1..=3u8 {
            assert!(
                l.hold_volumes(job(n), &[want("data", Mode::ReadOnly)], &available(), None)
                    .is_some(),
                "reader {n}"
            );
        }
        assert!(
            l.hold_volumes(job(9), &[want("data", Mode::ReadWrite)], &available(), None)
                .is_none(),
            "but a writer waits for all of them"
        );
    }

    /// The operator's statement about the directory outranks the job's
    /// wish for it.
    #[test]
    fn a_read_only_volume_refuses_a_write_claim() {
        let mut l = Leases::default();
        assert!(
            l.hold_volumes(job(1), &[want("ref", Mode::ReadWrite)], &available(), None)
                .is_none()
        );
        assert!(
            l.hold_volumes(job(1), &[want("ref", Mode::ReadOnly)], &available(), None)
                .is_some()
        );
    }

    /// Half a claim is worse than none: the job starts, writes half its
    /// state somewhere it did not mean to, and fails in its own code about
    /// its own paths.
    #[test]
    fn a_partial_claim_is_refused_whole() {
        let mut l = Leases::default();
        l.hold_volumes(
            job(1),
            &[want("cache", Mode::ReadWrite)],
            &available(),
            None,
        )
        .expect("first job takes cache");
        assert!(
            l.hold_volumes(
                job(2),
                &[
                    want("data", Mode::ReadWrite),
                    want("cache", Mode::ReadWrite)
                ],
                &available(),
                None,
            )
            .is_none()
        );
        let (who, _) = l.volume_holders("data");
        assert!(
            who.is_empty(),
            "and `data` was not quietly taken on the way"
        );
    }

    #[test]
    fn a_volume_this_node_does_not_have_is_refused() {
        let mut l = Leases::default();
        assert!(
            l.hold_volumes(job(1), &[want("nope", Mode::ReadOnly)], &available(), None)
                .is_none()
        );
    }

    /// **The property the whole phase exists for.** Releasing a claim is
    /// one table edit on the node that has the volume — so the instant a
    /// job is gone, however it went, the next job can take it. There is no
    /// counter to decrement, which is why there is nothing to get out of
    /// step with the mount.
    #[test]
    fn a_released_claim_is_immediately_available_with_no_counter_to_fix() {
        let mut l = Leases::default();
        l.hold_volumes(job(1), &[want("data", Mode::ReadWrite)], &available(), None)
            .expect("held");
        assert_eq!(l.release_volumes(&job(1)), 1);
        assert!(
            l.hold_volumes(job(2), &[want("data", Mode::ReadWrite)], &available(), None)
                .is_some(),
            "no reconciliation, no `deregister --force` — the row simply went"
        );
    }

    /// A job re-admitted after a retry must not block itself.
    #[test]
    fn re_claiming_is_idempotent_per_holder() {
        let mut l = Leases::default();
        let req = [want("data", Mode::ReadWrite)];
        assert!(l.hold_volumes(job(1), &req, &available(), None).is_some());
        assert!(
            l.hold_volumes(job(1), &req, &available(), None).is_some(),
            "the same holder re-asking is not a conflict with itself"
        );
        let (who, write) = l.volume_holders("data");
        assert_eq!(who.len(), 1, "and there is one row, not two");
        assert!(write);
    }

    /// A gang's claims expire with its lease; a job's never do.
    #[test]
    fn a_gang_claim_expires_with_its_lease() {
        let mut l = Leases::default();
        let gang = Holder::Gang(mesh_proto::GangId([9; 16]));
        l.hold_volumes(
            gang,
            &[want("data", Mode::ReadWrite)],
            &available(),
            Some(1_000),
        )
        .expect("held");
        l.hold_volumes(
            job(1),
            &[want("cache", Mode::ReadWrite)],
            &available(),
            None,
        )
        .expect("held");
        l.sweep_volumes(2_000);
        assert!(l.volume_holders("data").0.is_empty(), "the gang's went");
        assert_eq!(l.volume_holders("cache").0.len(), 1, "the job's stayed");
    }

    /// One volume asked for twice in one request is a contradiction, not a
    /// grant — and refusing it is cheaper than discovering which of the two
    /// modes won.
    #[test]
    fn one_volume_twice_in_one_request_is_refused() {
        let mut l = Leases::default();
        assert!(
            l.hold_volumes(
                job(1),
                &[want("data", Mode::ReadOnly), want("data", Mode::ReadWrite)],
                &available(),
                None,
            )
            .is_none()
        );
    }
}

#[cfg(test)]
mod host_port_tests {
    use super::*;
    use mesh_proto::PortRequests;

    fn job(n: u8) -> Holder {
        Holder::Job(mesh_proto::JobId([n; 16]))
    }
    fn range(min: u16, max: u16) -> mesh_proto::PortRange {
        mesh_proto::PortRange { min, max }
    }

    /// The ledger is not the only thing on the host.
    ///
    /// A port free in the table and taken on the machine — by a second
    /// meshware node, a database, an orphaned container — used to be handed
    /// out anyway. The job then died on `EADDRINUSE`, restarted, was handed
    /// the same number, and crash-looped, while `meshnode ports` insisted it
    /// was free.
    #[test]
    fn a_port_the_host_already_has_is_skipped() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:0");
        // 20000 and 20001 are "taken" by something outside this node.
        let held = |p: u16| p >= 20002;
        let got = l
            .hold_ports_checked(job(1), &reqs.0, &range(20000, 20010), &[], None, &held)
            .expect("an allocation");
        assert_eq!(got[0].host, 20002, "swept past what the host has");
    }

    /// A *demanded* port the host already has is a refusal, so the job
    /// spills to a node that can give it — not a bind failure at spawn.
    #[test]
    fn a_demanded_port_the_host_has_is_refused() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:20000!");
        let none_free = |_: u16| false;
        assert!(
            l.hold_ports_checked(job(1), &reqs.0, &range(20000, 20010), &[], None, &none_free)
                .is_none()
        );
        // And granted when the host really does have it.
        let all_free = |_: u16| true;
        assert_eq!(
            l.hold_ports_checked(job(1), &reqs.0, &range(20000, 20010), &[], None, &all_free)
                .expect("an allocation")[0]
                .host,
            20000
        );
    }

    /// A range the host has entirely is no allocation at all, rather than
    /// one that cannot be used.
    #[test]
    fn a_full_host_yields_nothing() {
        let mut l = Leases::default();
        let reqs = PortRequests::parse("http:0");
        assert!(
            l.hold_ports_checked(job(1), &reqs.0, &range(20000, 20003), &[], None, &|_| false)
                .is_none()
        );
    }

    /// The real check answers about a port this test is holding, which is
    /// the case it exists for.
    #[test]
    fn the_real_check_sees_a_socket_this_test_holds() {
        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind");
        let port = held.local_addr().expect("addr").port();
        assert!(!host_can_bind(port), "a bound port must not read as free");
        drop(held);
        assert!(host_can_bind(port), "and a released one must");
    }
}
