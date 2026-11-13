//! Accelerators: the device inventory, the device request, and the leases
//! that hand one to a job (C0–C9 §9).
//!
//! **Why this is not a `cap.*` tag.** The scalar capacity model (§9.1) was
//! an excellent decision and it does not extend here. A device is not a
//! scalar: it has an index, a model, a UUID, a memory size, a NUMA node and
//! a health state, and per-device leasing has to *refer* to a device
//! identity that a flat `cap.gpu.0.model=…` string namespace does not
//! really have. So the inventory is its own replicated entity, authored by
//! the node about itself, riding F0 as an ordinary op body — which is why
//! it costs no protocol minor.
//!
//! What *stays* on tags is the coarse filter: `cap.gpus=4` and
//! `cap.vram_mb=98304` (in [`crate::capacity`]). A selector needs to answer
//! "has a GPU" cheaply from replicated state without parsing an inventory,
//! and that is the tags' job. **The tags are the filter, not the model.**
//!
//! Requirements travel the other way round, as reserved job environment —
//! `MESHWARE_TASK_DEVICES`, JSON-encoded — for the reason M9 §5.3 gives and
//! §14.1 restates: a struct embedded in a wire message may never grow a
//! field, and [`crate::Requirements`] is embedded in `Request::GangOffer`.
//! The environment is a `Vec<(String, String)>` that already travels and is
//! extensible by construction.

use serde::{Deserialize, Serialize};

use crate::NodeId;

// ---------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------

/// What sort of device this is.
///
/// **An open enum, and that is the whole point of the shape.** `Other`
/// absorbs every accelerator nobody has thought of yet, so an FPGA farm
/// does not need a new entity — and, more importantly, does not need a new
/// *variant index*. An unknown variant index inside a body a peer does
/// decode would fail that decode; a string it does not recognise is simply
/// a device it will not match a selector against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DeviceKind {
    /// The default, because it is what nearly every device request means
    /// and typing `kind=gpu` for the common case is a tax.
    #[default]
    Gpu,
    Fpga,
    Nic,
    Other(String),
}

impl DeviceKind {
    pub fn as_str(&self) -> &str {
        match self {
            DeviceKind::Gpu => "gpu",
            DeviceKind::Fpga => "fpga",
            DeviceKind::Nic => "nic",
            DeviceKind::Other(s) => s.as_str(),
        }
    }

    /// Parse a kind from user input. Never fails: an unrecognised word is a
    /// kind this build has no name for, not an error, which is what makes
    /// the enum open in practice as well as in shape.
    pub fn parse(s: &str) -> DeviceKind {
        match s.trim().to_ascii_lowercase().as_str() {
            "gpu" => DeviceKind::Gpu,
            "fpga" => DeviceKind::Fpga,
            "nic" => DeviceKind::Nic,
            other => DeviceKind::Other(other.to_string()),
        }
    }
}

/// Whether a device can be given to work right now, and why not when it
/// cannot.
///
/// Health is *reported*, never inferred from absence: a device that has
/// fallen off the bus stops appearing in `nvidia-smi` altogether, and the
/// honest rendering of that is a device that left the inventory rather than
/// one that is `Unavailable` forever. What lands here is what the driver
/// says out loud — an ECC error it is reporting, a throttle it is holding,
/// a persistence-mode failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceHealth {
    #[default]
    Ok,
    /// Usable, with a caveat an operator should see. Still schedulable:
    /// refusing to place work on a GPU with one corrected ECC error would
    /// idle a working machine.
    Degraded { reason: String },
    /// Not schedulable. The driver is reporting something that means work
    /// placed here would fail, so placement skips it and `doctor` says so.
    Unavailable { reason: String },
}

impl DeviceHealth {
    /// May work be placed on a device in this state?
    pub fn schedulable(&self) -> bool {
        !matches!(self, DeviceHealth::Unavailable { .. })
    }

    pub fn as_str(&self) -> &str {
        match self {
            DeviceHealth::Ok => "ok",
            DeviceHealth::Degraded { .. } => "degraded",
            DeviceHealth::Unavailable { .. } => "unavailable",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            DeviceHealth::Ok => None,
            DeviceHealth::Degraded { reason } | DeviceHealth::Unavailable { reason } => {
                Some(reason.as_str())
            }
        }
    }
}

/// One accelerator, as the node that owns it describes it.
///
/// `index` is the driver's own ordinal — what `CUDA_VISIBLE_DEVICES` means
/// by a number — and it is the identity a lease refers to. `uuid` is the
/// stable identity across reboots and reorderings, which is what makes
/// "device 2 is the one that keeps throwing ECC errors" a statement about a
/// piece of hardware rather than about a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub index: u32,
    pub kind: DeviceKind,
    /// `nvidia`, `amd`, … lower-case, as the probe reported it.
    pub vendor: String,
    /// `NVIDIA GeForce RTX 4090`, verbatim from the driver.
    pub model: String,
    /// Driver-reported stable id (`GPU-…`), empty when the probe could not
    /// read one.
    pub uuid: String,
    /// Device memory in MB. 0 = unknown, treated as *unstated* the way an
    /// unprobed `cap.mem_mb` is: neutral, never disqualifying.
    pub mem_mb: u64,
    /// Host NUMA node this device hangs off, when the probe could tell.
    pub numa_node: Option<u32>,
    pub health: DeviceHealth,
}

/// Identity of one physical device on one node.
///
/// **The vendor's ordinal is only unique within that vendor.** A box with a
/// discrete NVIDIA card and an AMD integrated GPU has two devices that both
/// call themselves index 0 — which is the ordinary shape of a Ryzen desktop
/// with a graphics card in it, not an exotic one. Keying a lease on the
/// bare index lets a hold on one of them free the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceKey {
    /// Lower-case, as the probe reported it: `nvidia`, `amd`.
    pub vendor: String,
    pub kind: DeviceKind,
    /// The driver's own ordinal — still what `CUDA_VISIBLE_DEVICES` means.
    pub index: u32,
}

impl Device {
    /// What a lease refers to. See [`DeviceKey`].
    pub fn key(&self) -> DeviceKey {
        DeviceKey {
            vendor: self.vendor.clone(),
            kind: self.kind.clone(),
            index: self.index,
        }
    }

    /// Does this device satisfy `req`'s selectors (count aside)?
    pub fn matches(&self, req: &DeviceRequest) -> bool {
        if !self.health.schedulable() {
            return false;
        }
        if self.kind != req.kind {
            return false;
        }
        if let Some(model) = &req.model
            && !glob_match(model, &self.model)
        {
            return false;
        }
        if let Some(vendor) = &req.vendor
            && !glob_match(vendor, &self.vendor)
        {
            return false;
        }
        // Unknown device memory is neutral, the same rule an unprobed
        // `cap.mem_mb` gets: a node that could not read a figure is trusted
        // here and finds out at admission.
        !(req.vram_mb > 0 && self.mem_mb > 0 && self.mem_mb < req.vram_mb)
    }

    /// One line for a table: `0 nvidia RTX 4090 24GB`. Carries the vendor
    /// because the index alone does not identify a device ([`DeviceKey`]).
    pub fn summary(&self) -> String {
        // Integer GB would render a 512MB integrated GPU as `0GB`, which
        // reads as a broken device rather than a small one.
        let mem = match self.mem_mb {
            0 => String::new(),
            mb if mb < 1024 => format!(" {mb}MB"),
            mb => format!(" {}GB", mb / 1024),
        };
        format!("{} {} {}{}", self.index, self.vendor, self.model, mem)
    }
}

/// Case-insensitive match with `*` wildcards, which is what `--device
/// model=~4090` compiles to (`~x` means "contains x", i.e. `*x*`).
///
/// Deliberately not a regex: an operator typing a model fragment wants
/// substring matching, and a dependency-free matcher that does exactly that
/// cannot surprise them with a metacharacter they did not mean to type.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return value == pattern;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let hay = &value[pos..];
        let found = if i == 0 {
            hay.starts_with(part).then_some(0)
        } else {
            hay.find(part)
        };
        let Some(at) = found else {
            return false;
        };
        pos += at + part.len();
    }
    // A pattern not ending in `*` must consume the tail exactly.
    match parts.last() {
        Some(last) if !last.is_empty() => value.ends_with(last),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// The replicated entity
// ---------------------------------------------------------------------------

/// What one node has, as that node says so.
///
/// Replicated as [`crate::OpBody::DeviceInventoryPut`] and **rejected if
/// authored by anyone else**, exactly as capacity tags are self-asserted
/// today: a node may only describe its own hardware, because an operator
/// elsewhere asserting a GPU onto a box is how work lands where it cannot
/// run.
///
/// LWW by `published_at_micros`, one live record per node, republished only
/// on change — so it compacts to exactly one op per node and never rides a
/// per-job write path. Mirrors `ReservationsPut` and `DeploymentHealthPut`,
/// which is the third use of that shape and not a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInventory {
    pub node_id: NodeId,
    /// Index-ordered, so two announcements of the same hardware encode
    /// identically and the publisher can debounce on equality.
    pub devices: Vec<Device>,
    pub published_at_micros: u64,
}

impl DeviceInventory {
    pub fn new(node_id: NodeId, mut devices: Vec<Device>, at_micros: u64) -> DeviceInventory {
        devices.sort_by(|a, b| (&a.kind, a.index).cmp(&(&b.kind, b.index)));
        DeviceInventory {
            node_id,
            devices,
            published_at_micros: at_micros,
        }
    }

    /// Devices of a kind that are schedulable right now.
    pub fn healthy(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|d| d.health.schedulable())
    }

    /// How many devices match `req`'s selectors. The figure admission
    /// compares against `req.count`.
    pub fn matching(&self, req: &DeviceRequest) -> Vec<&Device> {
        self.devices.iter().filter(|d| d.matches(req)).collect()
    }

    /// The coarse filter tags this inventory implies (§9.1): a count and
    /// the largest single device's memory.
    ///
    /// The *largest*, not the total, and the difference matters: `--vram-mb
    /// 40000` asks for one device with 40 GB on it, and a box with four
    /// 12 GB cards has 48 GB and cannot run it. A filter that summed would
    /// send the job to a node that then refuses it.
    pub fn filter_tags(&self) -> Vec<String> {
        let gpus: Vec<&Device> = self
            .healthy()
            .filter(|d| d.kind == DeviceKind::Gpu)
            .collect();
        let mut tags = Vec::with_capacity(2);
        if !gpus.is_empty() {
            tags.push(format!("{}={}", crate::capacity::CAP_GPUS, gpus.len()));
            let vram = gpus.iter().map(|d| d.mem_mb).max().unwrap_or(0);
            if vram > 0 {
                tags.push(format!("{}={}", crate::capacity::CAP_VRAM_MB, vram));
            }
        }
        tags
    }
}

// ---------------------------------------------------------------------------
// The request (reserved job environment)
// ---------------------------------------------------------------------------

/// `--gpus N` / `--vram-mb M` / `--device kind=…,model=…`, JSON-encoded.
///
/// **JSON, and inside the environment, for one reason each.** The
/// environment because [`crate::Requirements`] is embedded in
/// `Request::GangOffer` and a struct in a wire message may never grow a
/// field (§14.1); JSON because this one *will* grow — MIG partitions,
/// topology constraints, fractional shares are all named as later work —
/// and a self-describing encoding is what lets an older executor read the
/// fields it knows and ignore the rest instead of failing the decode.
pub const REQ_DEVICES: &str = "MESHWARE_TASK_DEVICES";

/// What a task needs in the way of accelerators.
///
/// Zero `count` is *unstated*, which is how every pre-C5 job looks:
/// unstated device requirements fit everywhere, exactly as they did before
/// devices existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceRequest {
    /// How many devices, exclusively, unless `share` is set.
    pub count: u32,
    pub kind: DeviceKind,
    /// Model pattern (`*4090*`). `None` matches any model.
    pub model: Option<String>,
    pub vendor: Option<String>,
    /// Minimum memory **per device**, in MB (`--vram-mb`).
    pub vram_mb: u64,
    /// `--gpu-share`: this job tolerates co-tenants on its devices.
    ///
    /// **Opt-in, and never a default.** Silent sharing produces OOM
    /// failures that look like application bugs, on somebody else's job, in
    /// a framework that reports them three layers from the cause (§9.2).
    pub share: bool,
}

impl Default for DeviceRequest {
    fn default() -> Self {
        DeviceRequest {
            count: 0,
            kind: DeviceKind::Gpu,
            model: None,
            vendor: None,
            vram_mb: 0,
            share: false,
        }
    }
}

impl DeviceRequest {
    /// Nothing declared — the pre-C5 default, and the common case.
    pub fn is_unstated(&self) -> bool {
        self.count == 0 && self.vram_mb == 0 && self.model.is_none() && self.vendor.is_none()
    }

    /// How many devices this request actually reserves. A request that
    /// names a model or a VRAM floor without a count means *one*: `--device
    /// model=~4090` with no `--gpus` is a request for a 4090, singular.
    pub fn effective_count(&self) -> u32 {
        if self.count > 0 { self.count } else { 1 }
    }

    /// Render as the reserved environment entry, or nothing when unstated.
    pub fn to_env(&self) -> Vec<(String, String)> {
        if self.is_unstated() {
            return Vec::new();
        }
        match serde_json::to_string(self) {
            Ok(json) => vec![(REQ_DEVICES.to_string(), json)],
            Err(_) => Vec::new(),
        }
    }

    /// Recover a request from a job's environment. An unparseable value is
    /// ignored rather than turned into an accidental demand — the same rule
    /// [`crate::Requirements::from_env`] follows, and for the same reason.
    pub fn from_env(env: &[(String, String)]) -> Option<DeviceRequest> {
        env.iter()
            .find(|(k, _)| k == REQ_DEVICES)
            .and_then(|(_, v)| serde_json::from_str::<DeviceRequest>(v).ok())
            .filter(|r| !r.is_unstated())
    }

    /// Parse `--device kind=gpu,model=~4090,vendor=nvidia` onto a request.
    ///
    /// `~x` is sugar for `*x*`, because "contains" is what somebody typing
    /// a model fragment means and asking them to write globs would be a
    /// worse spelling of the same thought.
    pub fn apply_selector(&mut self, spec: &str) -> Result<(), String> {
        for term in spec.split(',').filter(|t| !t.trim().is_empty()) {
            let Some((key, value)) = term.split_once('=') else {
                return Err(format!("expected key=value in `--device`, got `{term}`"));
            };
            let value = value.trim();
            let pattern = || {
                if let Some(rest) = value.strip_prefix('~') {
                    format!("*{rest}*")
                } else {
                    value.to_string()
                }
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "kind" | "type" => self.kind = DeviceKind::parse(value),
                "model" => self.model = Some(pattern()),
                "vendor" => self.vendor = Some(pattern()),
                "count" | "n" => {
                    self.count = value
                        .parse()
                        .map_err(|_| format!("`--device count={value}` is not a number"))?;
                }
                "vram_mb" | "vram" => {
                    self.vram_mb = value
                        .parse()
                        .map_err(|_| format!("`--device vram_mb={value}` is not a number"))?;
                }
                other => return Err(format!("unknown `--device` key `{other}`")),
            }
        }
        Ok(())
    }

    /// One line for an explanation: `2×gpu model=*4090* ≥24000MB`.
    pub fn describe(&self) -> String {
        let mut s = format!("{}×{}", self.effective_count(), self.kind.as_str());
        if let Some(model) = &self.model {
            s.push_str(&format!(" model={model}"));
        }
        if let Some(vendor) = &self.vendor {
            s.push_str(&format!(" vendor={vendor}"));
        }
        if self.vram_mb > 0 {
            s.push_str(&format!(" ≥{}MB", self.vram_mb));
        }
        if self.share {
            s.push_str(" (shared)");
        }
        s
    }

    /// Could a node advertising these coarse tags *ever* satisfy this
    /// request? The cheap filter, answered from replicated tags without
    /// reading an inventory (§9.1) — which is exactly what the tags are for.
    ///
    /// A node that advertises no device tags at all is **not** a candidate
    /// for a device request. That is the one place the device model departs
    /// from "unknown is neutral", and deliberately: unknown memory means a
    /// probe failed on a machine that certainly has memory, while an absent
    /// `cap.gpus` on a node running this build means the machine has no
    /// GPU. Treating that as neutral would send every GPU job to every
    /// CPU box in the mesh.
    pub fn fits_tags<S: AsRef<str>>(&self, tags: &[S]) -> bool {
        if self.is_unstated() {
            return true;
        }
        let mut gpus = 0u32;
        let mut vram = 0u64;
        for tag in tags {
            match tag.as_ref().split_once('=') {
                Some((crate::capacity::CAP_GPUS, v)) => gpus = v.parse().unwrap_or(0),
                Some((crate::capacity::CAP_VRAM_MB, v)) => vram = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        if self.kind != DeviceKind::Gpu {
            // Only GPUs get a coarse tag; anything else is filtered against
            // the inventory alone, which is a smaller set of nodes and a
            // rarer request.
            return true;
        }
        gpus >= self.effective_count() && (self.vram_mb == 0 || vram == 0 || vram >= self.vram_mb)
    }
}

// ---------------------------------------------------------------------------
// The runtime environment (allocation side)
// ---------------------------------------------------------------------------

/// Prefix owned by the device runtime. Reserved on the job environment the
/// same way `MESHWARE_GANG_` is.
pub const DEVICE_ENV_PREFIX: &str = "MESHWARE_DEVICE_";

/// The indices this task was given, comma-separated, in the driver's own
/// numbering.
pub const DEVICE_INDICES: &str = "MESHWARE_DEVICE_INDICES";
/// The UUIDs of those devices, in the same order — the identity that
/// survives a reboot, for a task that wants to log what it ran on.
pub const DEVICE_UUIDS: &str = "MESHWARE_DEVICE_UUIDS";
/// How many devices this task holds.
pub const DEVICE_COUNT: &str = "MESHWARE_DEVICE_COUNT";

/// Is this environment key part of the device runtime set?
pub fn is_device_env(key: &str) -> bool {
    key.starts_with(DEVICE_ENV_PREFIX)
}

/// Vendor masks a task reads to find its devices, and the reason isolation
/// works without meshware knowing what CUDA is (§9.2).
///
/// All three are exported, always, because a box can have both vendors'
/// runtimes installed and a task that reads the wrong one would see every
/// device on the machine.
pub const CUDA_VISIBLE: &str = "CUDA_VISIBLE_DEVICES";
pub const HIP_VISIBLE: &str = "HIP_VISIBLE_DEVICES";
pub const ROCR_VISIBLE: &str = "ROCR_VISIBLE_DEVICES";

/// The environment an allocation puts in front of a task.
///
/// `devices` is the assignment, in the order the scheduler made it, so
/// `CUDA_VISIBLE_DEVICES` renumbers them `0..n` for the task exactly as the
/// vendor runtimes do.
pub fn allocation_env(devices: &[Device]) -> Vec<(String, String)> {
    // **Each vendor's mask lists only that vendor's devices.** The indices
    // are per-vendor ordinals, so pointing all three variables at one
    // combined list hands a HIP task the AMD device that happens to share
    // an ordinal with the NVIDIA card the task was actually given. On a box
    // with a discrete NVIDIA card and an AMD integrated GPU — an ordinary
    // desktop — that is a task allocated the 4090 and shown the iGPU.
    //
    // A vendor with no devices in the allocation still gets its mask set,
    // to the empty string: unset means *every device on the box*, which is
    // the hole `empty_allocation_env` exists to close.
    let of_vendor = |vendor: &str| -> String {
        devices
            .iter()
            .filter(|d| d.vendor.eq_ignore_ascii_case(vendor))
            .map(|d| d.index.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let amd = of_vendor("amd");
    // `MESHWARE_DEVICE_*` describe the whole allocation, whatever the mix.
    let indices: Vec<String> = devices.iter().map(|d| d.index.to_string()).collect();
    let uuids: Vec<&str> = devices.iter().map(|d| d.uuid.as_str()).collect();
    vec![
        (CUDA_VISIBLE.to_string(), of_vendor("nvidia")),
        (HIP_VISIBLE.to_string(), amd.clone()),
        (ROCR_VISIBLE.to_string(), amd),
        (DEVICE_INDICES.to_string(), indices.join(",")),
        (DEVICE_UUIDS.to_string(), uuids.join(",")),
        (DEVICE_COUNT.to_string(), devices.len().to_string()),
    ]
}

/// A task that asked for **no** devices gets the masks set to empty, not
/// left unset.
///
/// Unset means *every device on the box* to every vendor runtime there is,
/// which would make a CPU job on a GPU node able to allocate all of it —
/// and the isolation this milestone promises would hold only for jobs that
/// happened to ask. Isolation that depends on the isolated party asking for
/// it is not isolation.
pub fn empty_allocation_env() -> Vec<(String, String)> {
    vec![
        (CUDA_VISIBLE.to_string(), String::new()),
        (HIP_VISIBLE.to_string(), String::new()),
        (ROCR_VISIBLE.to_string(), String::new()),
    ]
}

// ---------------------------------------------------------------------------
// Live device telemetry (P5, `PressureV2`, gated at ACCEL_MINOR)
// ---------------------------------------------------------------------------

/// How busy one device is *right now* — the device half of the live
/// capacity model.
///
/// Deliberately not in the op-log and deliberately **not a new field on
/// [`crate::PressureSnapshot`]**: that struct is embedded in
/// `Response::Pressure`, so growing it would make every older peer misparse
/// every pressure reply. This is the fourth application of that rule in
/// this codebase (§9.1) and it will not be the last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevicePressure {
    pub index: u32,
    /// Whose numbering `index` is (1.35). `None` for a reading from a peer
    /// below [`crate::DEVICE_KEY_MINOR`], which sent the bare ordinal —
    /// and a bare ordinal names two devices on a box with an NVIDIA card
    /// and an AMD integrated GPU. See [`reading_for`].
    pub space: Option<OrdinalSpace>,
    /// Utilisation percent, 0–100.
    pub util_pct: Option<f64>,
    /// Free device memory in MB.
    pub mem_free_mb: Option<u64>,
    pub temp_c: Option<f64>,
    /// The driver is clamping clocks. Not a failure — a reason the job on
    /// it is slower than the identical job on the box next door.
    pub throttled: Option<bool>,
    /// Processes the driver reports on this device. The honest answer to
    /// "is anything using it", which utilisation alone is not: a GPU with a
    /// model loaded and idle between batches reads 0%.
    pub processes: u32,
}

impl DevicePressure {
    /// How busy this device is, in `0.0..=1.0`, for the placement term.
    /// `None` when nothing could be measured, which placement treats as
    /// neutral like every other unknown.
    pub fn busy_fraction(&self) -> Option<f64> {
        match (self.util_pct, self.processes) {
            (Some(util), procs) => {
                // A device with processes on it is at least half busy
                // however idle the sampler caught it: a loaded model
                // between batches reads 0% and is emphatically not free.
                let floor = if procs > 0 { 0.5 } else { 0.0 };
                Some((util / 100.0).clamp(0.0, 1.0).max(floor))
            }
            (None, procs) if procs > 0 => Some(1.0),
            _ => None,
        }
    }

    /// The device this reading is about, when the reading says.
    pub fn key(&self) -> Option<DeviceKey> {
        self.space.as_ref().map(|s| DeviceKey {
            vendor: s.vendor.clone(),
            kind: s.kind.clone(),
            index: self.index,
        })
    }
}

/// The vendor and kind a device ordinal is counted within — a
/// [`DeviceKey`] less its index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OrdinalSpace {
    /// Lower-case, as the probe reported it, the same as [`Device::vendor`].
    pub vendor: String,
    pub kind: DeviceKind,
}

impl OrdinalSpace {
    pub fn of(device: &Device) -> OrdinalSpace {
        OrdinalSpace {
            vendor: device.vendor.clone(),
            kind: device.kind.clone(),
        }
    }
}

/// The reading for `device`, out of one node's telemetry, or `None` when
/// there is no reading that is certainly its.
///
/// A reading that names its device matches that device only. One that does
/// not — from a peer below [`crate::DEVICE_KEY_MINOR`] — matches by ordinal
/// only where `inventory` (the same node's) has no other device with that
/// ordinal. Where it does, the reading could be either card's, and handing
/// the discrete card's load to the integrated one (or the other way round)
/// is worse than not knowing: unknown is neutral everywhere it is read.
pub fn reading_for<'a>(
    telemetry: &'a [DevicePressure],
    device: &Device,
    inventory: &[Device],
) -> Option<&'a DevicePressure> {
    let key = device.key();
    if let Some(exact) = telemetry.iter().find(|p| p.key().as_ref() == Some(&key)) {
        return Some(exact);
    }
    let shared = inventory
        .iter()
        .filter(|d| d.index == device.index)
        .nth(1)
        .is_some();
    if shared {
        return None;
    }
    telemetry
        .iter()
        .find(|p| p.space.is_none() && p.index == device.index)
}

/// [`DevicePressure`] as it travelled before 1.35: the bare ordinal.
///
/// Still the device half of [`PressureV2`] and [`crate::TelemetryEntry`],
/// field for field, because a peer below [`crate::DEVICE_KEY_MINOR`]
/// decodes those by position and may never be sent anything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevicePressureV1 {
    pub index: u32,
    pub util_pct: Option<f64>,
    pub mem_free_mb: Option<u64>,
    pub temp_c: Option<f64>,
    pub throttled: Option<bool>,
    pub processes: u32,
}

impl From<DevicePressure> for DevicePressureV1 {
    fn from(p: DevicePressure) -> Self {
        DevicePressureV1 {
            index: p.index,
            util_pct: p.util_pct,
            mem_free_mb: p.mem_free_mb,
            temp_c: p.temp_c,
            throttled: p.throttled,
            processes: p.processes,
        }
    }
}

impl From<DevicePressureV1> for DevicePressure {
    fn from(p: DevicePressureV1) -> Self {
        DevicePressure {
            index: p.index,
            space: None,
            util_pct: p.util_pct,
            mem_free_mb: p.mem_free_mb,
            temp_c: p.temp_c,
            throttled: p.throttled,
            processes: p.processes,
        }
    }
}

/// The live half of the capacity model, second edition (C5 P5).
///
/// An *appended pair* rather than a grown struct, gated at
/// [`crate::ACCEL_MINOR`]: a peer below that minor is never asked for it,
/// and its device pressure stays unknown — which placement already treats
/// as neutral.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressureV2 {
    /// Everything `Response::Pressure` already carried, unchanged, so one
    /// request answers both questions and a node that has V2 never needs
    /// to ask for V1 as well.
    pub base: crate::PressureSnapshot,
    pub devices: Vec<DevicePressureV1>,
}

impl PressureV2 {}

// ---------------------------------------------------------------------------
// Views (IPC)
// ---------------------------------------------------------------------------

/// One device as `meshnode nodes --devices` and Director show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRow {
    pub node: NodeId,
    pub node_name: String,
    pub index: u32,
    pub kind: String,
    pub vendor: String,
    pub model: String,
    pub uuid: String,
    pub mem_mb: u64,
    pub numa_node: Option<u32>,
    pub health: String,
    pub health_reason: Option<String>,
    /// The job holding this device on its own node, when this node is the
    /// one answering. `None` for a remote node — a lease is node-local and
    /// there is no honest way to read a remote one (the same limit C4's
    /// `meshnode top` column states).
    pub held_by: Option<String>,
    /// Live telemetry, when the node has been asked for it lately.
    pub util_pct: Option<f64>,
    pub mem_free_mb: Option<u64>,
    pub temp_c: Option<f64>,
    pub throttled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(index: u32, model: &str, mem_mb: u64) -> Device {
        Device {
            index,
            kind: DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: model.into(),
            uuid: format!("GPU-{index}"),
            mem_mb,
            numa_node: Some(0),
            health: DeviceHealth::Ok,
        }
    }

    #[test]
    fn request_round_trips_through_env() {
        let mut req = DeviceRequest {
            count: 2,
            vram_mb: 24_000,
            ..Default::default()
        };
        req.apply_selector("model=~4090").expect("selector");
        let env = req.to_env();
        assert_eq!(env[0].0, REQ_DEVICES);
        assert_eq!(DeviceRequest::from_env(&env), Some(req.clone()));
        // Mixed with ordinary env, order-independent.
        let mut mixed = vec![("HOME".to_string(), "/root".to_string())];
        mixed.extend(env);
        assert_eq!(DeviceRequest::from_env(&mixed), Some(req));
    }

    #[test]
    fn unstated_is_absent_not_zero() {
        let none = DeviceRequest::default();
        assert!(none.is_unstated());
        assert!(none.to_env().is_empty());
        assert_eq!(DeviceRequest::from_env(&[]), None);
        assert!(none.fits_tags(&["cap.cores=8"]));
    }

    #[test]
    fn a_newer_field_does_not_break_an_older_reader() {
        // The whole reason the request is JSON: a future key an older
        // executor has never heard of is ignored, not a decode failure.
        let env = vec![(
            REQ_DEVICES.to_string(),
            r#"{"count":1,"kind":"Gpu","mig_profile":"1g.10gb"}"#.to_string(),
        )];
        let req = DeviceRequest::from_env(&env).expect("decoded despite the unknown key");
        assert_eq!(req.count, 1);
    }

    #[test]
    fn tags_are_the_filter() {
        let inv = DeviceInventory::new(
            NodeId([7; 32]),
            vec![
                gpu(0, "NVIDIA GeForce RTX 4090", 24_564),
                gpu(1, "NVIDIA GeForce RTX 4090", 24_564),
            ],
            1,
        );
        assert_eq!(inv.filter_tags(), vec!["cap.gpus=2", "cap.vram_mb=24564"]);

        let two = DeviceRequest {
            count: 2,
            ..Default::default()
        };
        assert!(two.fits_tags(&inv.filter_tags()));
        let three = DeviceRequest {
            count: 3,
            ..Default::default()
        };
        assert!(!three.fits_tags(&inv.filter_tags()));
        // A CPU box is not a candidate for a GPU job, and this is the one
        // place unknown is *not* neutral.
        assert!(!two.fits_tags(&["cap.cores=64"]));
    }

    #[test]
    fn vram_filter_is_per_device_not_total() {
        // Four 12 GB cards have 48 GB and cannot run a 40 GB task.
        let inv = DeviceInventory::new(
            NodeId([1; 32]),
            (0..4).map(|i| gpu(i, "Tesla K80", 12_288)).collect(),
            1,
        );
        let big = DeviceRequest {
            count: 1,
            vram_mb: 40_000,
            ..Default::default()
        };
        assert!(!big.fits_tags(&inv.filter_tags()));
        assert!(inv.matching(&big).is_empty());
    }

    #[test]
    fn model_selector_matches_a_fragment() {
        let mut req = DeviceRequest::default();
        req.apply_selector("kind=gpu,model=~4090").expect("parse");
        assert_eq!(req.model.as_deref(), Some("*4090*"));
        assert!(gpu(0, "NVIDIA GeForce RTX 4090", 24_564).matches(&req));
        assert!(!gpu(0, "NVIDIA A100-SXM4-40GB", 40_960).matches(&req));
    }

    #[test]
    fn unavailable_devices_are_not_matched_or_advertised() {
        let mut bad = gpu(1, "NVIDIA GeForce RTX 4090", 24_564);
        bad.health = DeviceHealth::Unavailable {
            reason: "fell off the bus".into(),
        };
        let inv = DeviceInventory::new(
            NodeId([2; 32]),
            vec![gpu(0, "NVIDIA GeForce RTX 4090", 24_564), bad],
            1,
        );
        assert_eq!(inv.filter_tags(), vec!["cap.gpus=1", "cap.vram_mb=24564"]);
        assert_eq!(inv.matching(&DeviceRequest::default()).len(), 1);
        // Degraded is still schedulable: idling a working machine over one
        // corrected ECC error is the wrong trade.
        let mut degraded = gpu(2, "NVIDIA GeForce RTX 4090", 24_564);
        degraded.health = DeviceHealth::Degraded {
            reason: "1 corrected ECC error".into(),
        };
        assert!(degraded.health.schedulable());
    }

    #[test]
    fn allocation_env_masks_every_vendor() {
        let env = allocation_env(&[gpu(2, "x", 1), gpu(3, "y", 1)]);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        // Two NVIDIA cards: CUDA sees them, and the AMD masks are set to
        // empty rather than left unset — unset means *every device on the
        // box* to a vendor runtime.
        assert_eq!(get(CUDA_VISIBLE), "2,3");
        assert_eq!(get(HIP_VISIBLE), "");
        assert_eq!(get(ROCR_VISIBLE), "");
        assert_eq!(get(DEVICE_COUNT), "2");
        assert_eq!(get(DEVICE_UUIDS), "GPU-2,GPU-3");
        // And a job that asked for nothing is masked to nothing.
        let none = empty_allocation_env();
        assert!(none.iter().all(|(_, v)| v.is_empty()));
    }

    #[test]
    fn a_vendor_mask_never_names_another_vendors_ordinal() {
        // The case a desktop actually presents: a discrete NVIDIA card and
        // an AMD integrated GPU, both calling themselves index 0. A job
        // given the NVIDIA card must not have HIP pointed at "0", because
        // that is the iGPU — a different piece of hardware that may already
        // be allocated to somebody else.
        let mut amd = gpu(0, "RAPHAEL", 512);
        amd.vendor = "amd".into();
        amd.uuid = "0x0".into();
        let nvidia = gpu(0, "NVIDIA GeForce RTX 3080", 10_240);

        let env = allocation_env(&[nvidia]);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get(CUDA_VISIBLE), "0");
        assert_eq!(get(HIP_VISIBLE), "", "this ordinal is the AMD iGPU");

        // And the mixed allocation names each ordinal to its own runtime.
        let both = allocation_env(&[gpu(0, "NVIDIA GeForce RTX 3080", 10_240), amd]);
        let get2 = |k: &str| {
            both.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get2(CUDA_VISIBLE), "0");
        assert_eq!(get2(HIP_VISIBLE), "0");
        assert_eq!(get2(DEVICE_COUNT), "2");
    }

    #[test]
    fn glob_matches_what_an_operator_would_expect() {
        assert!(glob_match("*4090*", "NVIDIA GeForce RTX 4090"));
        assert!(glob_match("nvidia*", "NVIDIA GeForce RTX 4090"));
        assert!(glob_match("*4090", "NVIDIA GeForce RTX 4090"));
        assert!(!glob_match("*4080*", "NVIDIA GeForce RTX 4090"));
        assert!(glob_match("nvidia", "nvidia"));
        assert!(!glob_match("nvidia", "nvidia corp"));
    }

    #[test]
    fn busy_fraction_never_calls_a_loaded_device_idle() {
        let idle = DevicePressure {
            index: 0,
            space: None,
            util_pct: Some(0.0),
            mem_free_mb: Some(24_000),
            temp_c: Some(35.0),
            throttled: Some(false),
            processes: 0,
        };
        assert_eq!(idle.busy_fraction(), Some(0.0));
        // A model loaded and idle between batches reads 0% and is not free.
        let loaded = DevicePressure {
            processes: 1,
            ..idle.clone()
        };
        assert_eq!(loaded.busy_fraction(), Some(0.5));
        let unmeasured = DevicePressure {
            util_pct: None,
            processes: 0,
            ..idle
        };
        assert_eq!(unmeasured.busy_fraction(), None);
    }

    /// The bytes a pre-1.35 peer sends and expects inside `PressureV2` and
    /// `Telemetry`. Postcard encodes fields by position, so this layout may
    /// never change: a field added here makes every older peer misread
    /// every reading after the first.
    #[test]
    fn the_v1_reading_keeps_its_wire_layout() {
        let reading = DevicePressureV1 {
            index: 1,
            util_pct: Some(50.0),
            mem_free_mb: Some(1024),
            temp_c: None,
            throttled: Some(true),
            processes: 2,
        };
        let golden: &[u8] = &[
            1, // index
            1, 0, 0, 0, 0, 0, 0, 0x49, 0x40, // Some(50.0)
            1, 0x80, 0x08, // Some(1024)
            0,    // None
            1, 1, // Some(true)
            2, // processes
        ];
        assert_eq!(postcard::to_stdvec(&reading).expect("encode"), golden);
        assert_eq!(
            postcard::from_bytes::<DevicePressureV1>(golden).expect("decode"),
            reading
        );
    }

    fn reading(index: u32, space: Option<&str>, util: f64) -> DevicePressure {
        DevicePressure {
            index,
            space: space.map(|vendor| OrdinalSpace {
                vendor: vendor.into(),
                kind: DeviceKind::Gpu,
            }),
            util_pct: Some(util),
            mem_free_mb: None,
            temp_c: None,
            throttled: None,
            processes: 0,
        }
    }

    fn amd(index: u32) -> Device {
        Device {
            vendor: "amd".into(),
            model: "Radeon 680M".into(),
            ..gpu(index, "", 512)
        }
    }

    /// The desktop the whole field exists for: a discrete NVIDIA card and an
    /// AMD integrated GPU, both ordinal 0. Each gets its own reading.
    #[test]
    fn a_named_reading_goes_to_its_own_card_only() {
        let inventory = [gpu(0, "RTX 4090", 24_576), amd(0)];
        let telemetry = [
            reading(0, Some("nvidia"), 97.0),
            reading(0, Some("amd"), 3.0),
        ];
        let util = |d: &Device| reading_for(&telemetry, d, &inventory).and_then(|p| p.util_pct);
        assert_eq!(util(&inventory[0]), Some(97.0));
        assert_eq!(util(&inventory[1]), Some(3.0));

        // The NVIDIA card alone reporting leaves the AMD one unknown.
        let telemetry = [reading(0, Some("nvidia"), 97.0)];
        assert!(reading_for(&telemetry, &inventory[1], &inventory).is_none());
    }

    /// A pre-1.35 reading names only the ordinal: it is trusted where the
    /// ordinal is unambiguous, and nowhere else.
    #[test]
    fn a_bare_ordinal_is_used_only_where_it_names_one_card() {
        let single = [gpu(0, "A100", 80_000), gpu(1, "A100", 80_000)];
        let telemetry = [reading(1, None, 40.0)];
        assert_eq!(
            reading_for(&telemetry, &single[1], &single).and_then(|p| p.util_pct),
            Some(40.0)
        );
        assert!(reading_for(&telemetry, &single[0], &single).is_none());

        let mixed = [gpu(0, "RTX 4090", 24_576), amd(0)];
        let telemetry = [reading(0, None, 97.0)];
        assert!(reading_for(&telemetry, &mixed[0], &mixed).is_none());
        assert!(reading_for(&telemetry, &mixed[1], &mixed).is_none());
    }

    #[test]
    fn a_reading_crosses_the_old_wire_without_its_space() {
        let named = reading(2, Some("nvidia"), 10.0);
        let old: DevicePressureV1 = named.clone().into();
        let back: DevicePressure = old.into();
        assert_eq!(
            back,
            DevicePressure {
                space: None,
                ..named.clone()
            }
        );
        let bytes = postcard::to_stdvec(&named).expect("encode");
        assert_eq!(
            postcard::from_bytes::<DevicePressure>(&bytes).expect("decode"),
            named
        );
    }

    #[test]
    fn inventory_encodes_identically_for_identical_hardware() {
        let a = DeviceInventory::new(NodeId([3; 32]), vec![gpu(1, "m", 1), gpu(0, "m", 1)], 5);
        let b = DeviceInventory::new(NodeId([3; 32]), vec![gpu(0, "m", 1), gpu(1, "m", 1)], 5);
        assert_eq!(a, b, "order-independent, so republish can debounce");
    }
}
