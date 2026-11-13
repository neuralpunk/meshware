//! Local accelerator discovery and telemetry (C0–C9 §9.2, C5 P2 and P5).
//!
//! **Shells out before it links, and this is a decision rather than a
//! shortcut.** `nvidia-smi` and `rocm-smi` ship with the drivers, are on
//! every box that has the hardware, and cost a fork. Linking NVML would
//! mean a `dlopen` at best and a musl static-linking problem at worst, on a
//! binary whose size and portability are features. A missing binary is
//! simply *no devices of that vendor*, which is exactly right for the
//! ninety-nine machines out of a hundred that have none.
//!
//! Every probe is best-effort in the same way [`crate::capacity`]'s are: a
//! failure reports nothing rather than zero, and nothing is *unknown*,
//! which is neutral. The one place that rule is deliberately broken is the
//! coarse filter (`cap.gpus`) — see [`mesh_proto::DeviceRequest::fits_tags`]
//! for why an absent GPU tag means "no GPU" rather than "unmeasured".
//!
//! meshware schedules and isolates devices. It does not know what CUDA is,
//! it does not link a framework, and it never will: everything below reads
//! a driver's own reporting tool and hands the numbers to a bin-packer.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mesh_proto::{Device, DeviceHealth, DeviceKind, DevicePressure, OrdinalSpace};
use tracing::debug;

/// How long a probe may take before it is abandoned.
///
/// `nvidia-smi` on a healthy box answers in tens of milliseconds. On a box
/// whose driver is wedged — the exact case this milestone has to survive —
/// it can block forever, and a probe that blocks forever on the gossip tick
/// would take the node's whole announcement path with it. Five seconds is
/// two orders of magnitude of headroom and still bounded.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe every vendor and return this node's device inventory, index-ordered.
///
/// The two probes are independent: a box with an NVIDIA card and a broken
/// ROCm install reports the NVIDIA card.
pub fn detect() -> Vec<Device> {
    let mut devices = nvidia_devices();
    devices.extend(amd_devices());
    // Sort and dedup on the *full* identity, vendor included. Vendor
    // ordinals restart at zero, so a discrete NVIDIA card and an AMD
    // integrated GPU are both "device 0" and sorting on `(kind, index)`
    // alone would call them the same slot. Dedup still matters within a
    // vendor: a probe that lists a card twice must not double the count a
    // node advertises, because `cap.gpus` is what admission filters on.
    devices.sort_by_key(mesh_proto::Device::key);
    devices.dedup_by_key(|d| d.key());
    devices
}

/// Live per-device telemetry for the devices this node has (C5 P5).
///
/// Every reading names its vendor and kind, the same values the inventory
/// probe gives the device, so a reading is matched to exactly one card even
/// where two vendors both have a device 0 ([`mesh_proto::reading_for`]).
pub fn pressure() -> Vec<DevicePressure> {
    let mut out = nvidia_pressure();
    out.extend(amd_pressure());
    out.sort_by(|a, b| (a.index, &a.space).cmp(&(b.index, &b.space)));
    out
}

fn gpu_space(vendor: &str) -> Option<OrdinalSpace> {
    Some(OrdinalSpace {
        vendor: vendor.into(),
        kind: DeviceKind::Gpu,
    })
}

// ---------------------------------------------------------------------------
// NVIDIA
// ---------------------------------------------------------------------------

/// The static query. Deliberately one invocation for every field: a second
/// fork per device would turn an eight-GPU box's boot probe into nine.
const NVIDIA_STATIC_QUERY: &str =
    "index,name,uuid,memory.total,pci.bus_id,ecc.errors.uncorrected.volatile.total";

fn nvidia_devices() -> Vec<Device> {
    let Some(out) = run(
        "nvidia-smi",
        &[
            &format!("--query-gpu={NVIDIA_STATIC_QUERY}"),
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 5 {
                return None;
            }
            let index: u32 = f[0].parse().ok()?;
            let bus_id = f[4];
            Some(Device {
                index,
                kind: DeviceKind::Gpu,
                vendor: "nvidia".into(),
                model: f[1].to_string(),
                uuid: f[2].to_string(),
                mem_mb: f[3].parse().unwrap_or(0),
                numa_node: numa_node_of(bus_id),
                health: nvidia_health(f.get(5).copied().unwrap_or("")),
            })
        })
        .collect()
}

/// Turn the driver's ECC reporting into a health state (C5 P6).
///
/// **Uncorrected** errors only. A corrected error is the hardware doing its
/// job and is not a reason to idle a working card; an uncorrected one has
/// already given somebody's process wrong numbers, and the next job placed
/// there will get wrong numbers too. `N/A` — a card without ECC, or a
/// driver that will not say — is unknown, and unknown is neutral.
fn nvidia_health(ecc_uncorrected: &str) -> DeviceHealth {
    match ecc_uncorrected.trim().parse::<u64>() {
        Ok(0) => DeviceHealth::Ok,
        Ok(n) => DeviceHealth::Unavailable {
            reason: format!("{n} uncorrected ECC error(s)"),
        },
        Err(_) => DeviceHealth::Ok,
    }
}

/// The live query, less the clock-reasons field (see below).
const NVIDIA_PRESSURE_QUERY: &str = "index,utilization.gpu,memory.free,temperature.gpu";

/// What the driver calls the clock-reasons bitmask, newest name first.
///
/// NVIDIA renamed `clocks_throttle_reasons` to `clocks_event_reasons`
/// and kept the old name as an alias: a current driver answers to both
/// and labels the column with the new one, and one from before the rename
/// knows only the old one. `nvidia-smi` refuses a whole query over one
/// field it does not know, so asking by the wrong name loses every
/// reading on the card — utilisation and temperature too — not just this
/// one.
const NVIDIA_REASONS_FIELDS: [&str; 2] = [
    "clocks_event_reasons.active",
    "clocks_throttle_reasons.active",
];

/// Which of [`NVIDIA_REASONS_FIELDS`] this machine's driver last answered
/// to, so a sample costs one fork once the answer is known.
static NVIDIA_REASONS_FIELD: AtomicUsize = AtomicUsize::new(0);

fn nvidia_pressure() -> Vec<DevicePressure> {
    let first = NVIDIA_REASONS_FIELD.load(Ordering::Relaxed);
    let mut out = None;
    for i in [first, 1 - first] {
        let query = format!(
            "--query-gpu={NVIDIA_PRESSURE_QUERY},{}",
            NVIDIA_REASONS_FIELDS[i]
        );
        match run_probe("nvidia-smi", &[&query, "--format=csv,noheader,nounits"]) {
            Ok(text) => {
                NVIDIA_REASONS_FIELD.store(i, Ordering::Relaxed);
                out = Some(text);
                break;
            }
            // Refused: most likely the field name, so ask by the other one.
            // A driver upgrade can move the answer either way while the
            // daemon runs, which is why this is tried every time the
            // remembered name fails and not only once.
            Err(ProbeFailure::Exited) => {}
            // Missing or wedged: the other name would fare no better, and a
            // second five-second wait would double the stall.
            Err(_) => break,
        }
    }
    let Some(out) = out else {
        return Vec::new();
    };
    // Which devices actually have compute processes on them. Utilisation
    // alone is not the answer to "is anything using it": a loaded model
    // between batches reads 0% and is emphatically not free.
    parse_nvidia_pressure(&out, &nvidia_process_counts())
}

/// The parsing half of [`nvidia_pressure`], one line per card:
/// `index, util %, free MB, °C, reasons mask`.
fn parse_nvidia_pressure(out: &str, procs: &BTreeMap<u32, u32>) -> Vec<DevicePressure> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 4 {
                return None;
            }
            let index: u32 = f[0].parse().ok()?;
            Some(DevicePressure {
                index,
                space: gpu_space("nvidia"),
                util_pct: f[1].parse().ok(),
                mem_free_mb: f[2].parse().ok(),
                temp_c: f[3].parse().ok(),
                throttled: f.get(4).and_then(|v| nvidia_throttled(v)),
                processes: procs.get(&index).copied().unwrap_or(0),
            })
        })
        .collect()
}

/// The clock-event reasons (NVML's `nvmlClocksEventReason*` bits) that mean
/// the driver is holding the card below the speed it would otherwise run.
///
/// **Not every reason is a slowdown.** Bit `0x1` is *GPU idle*: the driver
/// drops the clocks of a card with nothing on it, and reports that as a
/// reason. Counting it called every idle NVIDIA card "throttling" — the one
/// state in which a card is certainly not slowing anybody down. The other
/// reasons left out are ones somebody chose (application clock limits
/// `0x2`, sync boost `0x10`) or a display being kept flicker-free
/// (`0x100`), none of which is a card running hot or short of power.
const NVIDIA_SLOWDOWN_REASONS: u64 = 0x04 // software power cap
    | 0x08 // hardware slowdown
    | 0x20 // software thermal slowdown
    | 0x40 // hardware thermal slowdown
    | 0x80; // hardware power brake

/// `clocks_throttle_reasons.active`, a hex bitmask, as "is this card being
/// slowed down". `None` when the driver would not say (`[N/A]`, `[Not
/// Supported]`). Which reason it is, is a question for `nvidia-smi -q -d
/// PERFORMANCE`, not for a scheduler.
fn nvidia_throttled(mask: &str) -> Option<bool> {
    let mask = u64::from_str_radix(mask.trim().trim_start_matches("0x"), 16).ok()?;
    Some(mask & NVIDIA_SLOWDOWN_REASONS != 0)
}

fn nvidia_process_counts() -> BTreeMap<u32, u32> {
    let mut counts = BTreeMap::new();
    let Some(out) = run(
        "nvidia-smi",
        &[
            "--query-compute-apps=gpu_bus_id",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return counts;
    };
    // The bus id is the join key back to the device list, because
    // `--query-compute-apps` will not report the ordinal.
    let by_bus: BTreeMap<String, u32> = run(
        "nvidia-smi",
        &[
            "--query-gpu=index,pci.bus_id",
            "--format=csv,noheader,nounits",
        ],
    )
    .map(|list| {
        list.lines()
            .filter_map(|l| {
                let (i, bus) = l.split_once(',')?;
                Some((bus.trim().to_ascii_lowercase(), i.trim().parse().ok()?))
            })
            .collect()
    })
    .unwrap_or_default();
    for line in out.lines() {
        if let Some(index) = by_bus.get(&line.trim().to_ascii_lowercase()) {
            *counts.entry(*index).or_default() += 1;
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// AMD
// ---------------------------------------------------------------------------

fn amd_devices() -> Vec<Device> {
    let Some(out) = run(
        "rocm-smi",
        &[
            "--showproductname",
            "--showmeminfo",
            "vram",
            "--showuniqueid",
            "--showbus",
            "--json",
        ],
    ) else {
        return Vec::new();
    };
    parse_amd_json(&out)
}

/// The parsing half of [`amd_devices`], split out so it can be tested
/// against real `rocm-smi` output on a machine with no AMD driver.
fn parse_amd_json(out: &str) -> Vec<Device> {
    let Ok(cards) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(out) else {
        return Vec::new();
    };
    cards
        .into_iter()
        .filter_map(|(card, v)| {
            let index: u32 = card.strip_prefix("card")?.parse().ok()?;
            // `rocm-smi` reports a field it cannot read as the *string*
            // `"N/A"`, which is `Some("N/A")` — so a plain `.or_else` chain
            // over these never reaches its fallbacks and a card ends up
            // named "N/A". Absent and unreadable are the same thing here.
            let field = |name: &str| -> Option<String> {
                let raw = v.as_object()?.iter().find_map(|(k, val)| {
                    k.to_ascii_lowercase()
                        .contains(&name.to_ascii_lowercase())
                        .then(|| val.as_str().unwrap_or_default().trim().to_string())
                })?;
                if raw.is_empty() || raw.eq_ignore_ascii_case("n/a") {
                    return None;
                }
                Some(raw)
            };
            let bus = field("PCI Bus").unwrap_or_default();
            Some(Device {
                index,
                kind: DeviceKind::Gpu,
                vendor: "amd".into(),
                // SKU before model: `Card SKU` is a name a person recognises
                // ("RAPHAEL"), `Card Model` is a raw PCI device id ("0x164e").
                model: field("Card Series")
                    .or_else(|| field("Card SKU"))
                    .or_else(|| field("Card model"))
                    .unwrap_or_else(|| "AMD GPU".into()),
                uuid: field("Unique ID").unwrap_or_default(),
                // rocm-smi reports VRAM in bytes.
                mem_mb: field("Total Memory")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|bytes| bytes / (1024 * 1024))
                    .unwrap_or(0),
                numa_node: numa_node_of(&bus),
                health: DeviceHealth::Ok,
            })
        })
        .collect()
}

fn amd_pressure() -> Vec<DevicePressure> {
    let Some(out) = run(
        "rocm-smi",
        &["--showuse", "--showmemuse", "--showtemp", "--json"],
    ) else {
        return Vec::new();
    };
    let Ok(cards) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&out) else {
        return Vec::new();
    };
    cards
        .into_iter()
        .filter_map(|(card, v)| {
            let index: u32 = card.strip_prefix("card")?.parse().ok()?;
            let field = |name: &str| -> Option<f64> {
                v.as_object()?.iter().find_map(|(k, val)| {
                    k.to_ascii_lowercase()
                        .contains(&name.to_ascii_lowercase())
                        .then(|| val.as_str()?.trim().parse::<f64>().ok())
                        .flatten()
                })
            };
            Some(DevicePressure {
                index,
                space: gpu_space("amd"),
                util_pct: field("GPU use (%)").or_else(|| field("GPU use")),
                mem_free_mb: None,
                temp_c: field("Temperature (Sensor junction)")
                    .or_else(|| field("Temperature (Sensor edge)")),
                throttled: None,
                processes: 0,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// The host NUMA node a PCI device hangs off, from sysfs.
///
/// Read here rather than asked of the vendor tool because the kernel is the
/// authority and both vendors report the bus id in the same shape. `-1`
/// means the kernel has no affinity information, which is a real answer and
/// not a node number.
fn numa_node_of(bus_id: &str) -> Option<u32> {
    let bdf = normalise_bdf(bus_id)?;
    let text = std::fs::read_to_string(
        Path::new("/sys/bus/pci/devices")
            .join(bdf)
            .join("numa_node"),
    )
    .ok()?;
    match text.trim().parse::<i32>() {
        Ok(n) if n >= 0 => u32::try_from(n).ok(),
        _ => None,
    }
}

/// `00000000:01:00.0` (nvidia-smi) or `0000:01:00.0` (rocm-smi) → the
/// sysfs spelling, `0000:01:00.0`.
fn normalise_bdf(bus_id: &str) -> Option<String> {
    let bus_id = bus_id.trim().to_ascii_lowercase();
    if bus_id.is_empty() {
        return None;
    }
    let parts: Vec<&str> = bus_id.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    // The domain is zero-padded to eight digits by nvidia-smi and to four
    // by sysfs; take the low four.
    let domain = parts[0];
    let domain = &domain[domain.len().saturating_sub(4)..];
    Some(format!("{domain}:{}:{}", parts[1], parts[2]))
}

/// Run a probe, bounded. `None` when the binary is missing (the common
/// case — a machine with no accelerators), when it fails, or when it does
/// not answer inside [`PROBE_TIMEOUT`].
fn run(bin: &str, args: &[&str]) -> Option<String> {
    run_probe(bin, args).ok()
}

/// Why a probe gave no answer. Only [`ProbeFailure::Exited`] is worth
/// asking again differently: the tool ran and refused the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailure {
    /// Not installed, or could not be started.
    Missing,
    /// Ran and exited non-zero.
    Exited,
    /// Did not answer within [`PROBE_TIMEOUT`], and was killed.
    TimedOut,
    /// Its output could not be read.
    Unreadable,
}

fn run_probe(bin: &str, args: &[&str]) -> Result<String, ProbeFailure> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        // Not an error worth logging at anything above trace: most machines
        // in most meshes have no accelerator and never will.
        Err(_) => return Err(ProbeFailure::Missing),
    };
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    debug!(probe = bin, "device probe exited {status}");
                    return Err(ProbeFailure::Exited);
                }
                break;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                // A wedged driver. Kill it and report no devices rather
                // than hold the announcement path open forever.
                debug!(probe = bin, "device probe timed out; killing");
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeFailure::TimedOut);
            }
            Err(_) => return Err(ProbeFailure::Unreadable),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|_| ProbeFailure::Unreadable)?;
    String::from_utf8(out.stdout).map_err(|_| ProbeFailure::Unreadable)
}

/// Are any vendor probes installed on this machine at all? `doctor` asks,
/// so that "no devices" can be told apart from "no driver tools".
pub fn probes_available() -> Vec<&'static str> {
    ["nvidia-smi", "rocm-smi"]
        .into_iter()
        .filter(|bin| which(bin).is_some())
        .collect()
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An idle card is not a throttled one. `0x1` is what `nvidia-smi`
    /// reports for a GeForce with nothing on it, and it was read as
    /// throttling, so every idle NVIDIA card in a mesh was flagged.
    #[test]
    fn only_a_power_or_thermal_clamp_counts_as_throttling() {
        for idle in [
            "0x0000000000000000",
            "0x0000000000000001", // idle
            "0x0000000000000002", // application clock setting
            "0x0000000000000010", // sync boost
            "0x0000000000000101", // idle, with a display attached
        ] {
            assert_eq!(nvidia_throttled(idle), Some(false), "{idle}");
        }
        for slowed in [
            "0x0000000000000004", // software power cap
            "0x0000000000000008", // hardware slowdown
            "0x0000000000000020", // software thermal
            "0x0000000000000041", // hardware thermal, reported with idle
            "0x0000000000000080", // power brake
        ] {
            assert_eq!(nvidia_throttled(slowed), Some(true), "{slowed}");
        }
        assert_eq!(nvidia_throttled("[N/A]"), None);
        assert_eq!(nvidia_throttled("[Not Supported]"), None);
    }

    /// The line `nvidia-smi` prints for an idle RTX 3080 on a current
    /// driver (Fedora 44, 2026-09-17), in the shape the probe asks for.
    #[test]
    fn an_idle_card_reads_as_idle_and_not_throttled() {
        let procs = BTreeMap::new();
        let cards = parse_nvidia_pressure("0, 0, 9812, 25, 0x0000000000000001\n", &procs);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.util_pct, Some(0.0));
        assert_eq!(card.temp_c, Some(25.0));
        assert_eq!(card.throttled, Some(false));
        assert_eq!(card.busy_fraction(), Some(0.0));
        assert_eq!(card.key().map(|k| k.vendor), Some("nvidia".to_string()));

        // A driver that will not say leaves the flag unknown, and the rest
        // of the reading still stands.
        let cards = parse_nvidia_pressure("1, 12, [N/A], 40, [N/A]\n", &procs);
        assert_eq!(cards[0].throttled, None);
        assert_eq!(cards[0].mem_free_mb, None);
        assert_eq!(cards[0].util_pct, Some(12.0));
    }

    /// Only a refusal is worth asking again by the other field name.
    #[test]
    fn a_missing_probe_is_told_apart_from_a_refusal() {
        assert_eq!(
            run_probe("meshware-no-such-probe-binary", &[]),
            Err(ProbeFailure::Missing)
        );
        assert_eq!(run_probe("false", &[]), Err(ProbeFailure::Exited));
        assert_eq!(run_probe("true", &[]).as_deref(), Ok(""));
    }

    #[test]
    fn a_machine_without_drivers_reports_no_devices_not_an_error() {
        // Whatever this test machine is, the contract is the same: probing
        // never panics and never fabricates hardware.
        let devices = detect();
        for d in &devices {
            assert_eq!(d.kind, DeviceKind::Gpu);
            assert!(!d.vendor.is_empty());
        }
    }

    /// Verbatim from an AMD Raphael integrated GPU — a Ryzen 7000 desktop
    /// with a discrete NVIDIA card in it, which is an ordinary machine and
    /// not an exotic one. Every field this fixture exercises was wrong in
    /// the first cut: the series reads `N/A`, the model is a raw PCI id,
    /// and the only human-readable name is the SKU.
    const RAPHAEL_JSON: &str = r#"{"card0": {"Unique ID": "0x0", "PCI Bus": "0000:15:00.0",
        "VRAM Total Memory (B)": "536870912", "VRAM Total Used Memory (B)": "16650240",
        "Card Series": "N/A", "Card Model": "0x164e",
        "Card Vendor": "Advanced Micro Devices, Inc. [AMD/ATI]", "Card SKU": "RAPHAEL",
        "Subsystem ID": "-0x3000", "Device Rev": "0xc1", "Node ID": "1", "GUID": "15964",
        "GFX Version": "gfx1036"}}"#;

    #[test]
    fn an_unreadable_rocm_field_falls_through_to_one_that_reads() {
        let devices = parse_amd_json(RAPHAEL_JSON);
        assert_eq!(devices.len(), 1);
        // "N/A" is `Some("N/A")` from the JSON and used to win the
        // `.or_else` chain outright, naming the card "N/A".
        assert_eq!(devices[0].model, "RAPHAEL");
        assert_eq!(devices[0].vendor, "amd");
        assert_eq!(devices[0].mem_mb, 512);
    }

    #[test]
    fn a_sub_gigabyte_device_does_not_report_zero() {
        let devices = parse_amd_json(RAPHAEL_JSON);
        // Integer GB rendered this 512MB iGPU as `0GB`, which reads as a
        // broken device rather than a small one.
        assert!(
            devices[0].summary().contains("512MB"),
            "got {:?}",
            devices[0].summary()
        );
    }

    #[test]
    fn two_vendors_at_index_zero_are_two_devices_not_one() {
        // The shape that started this: an NVIDIA discrete card and an AMD
        // integrated GPU both call themselves index 0.
        let nvidia = Device {
            index: 0,
            kind: DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: "NVIDIA GeForce RTX 3080".into(),
            uuid: "GPU-ff6a780d".into(),
            mem_mb: 10240,
            numa_node: None,
            health: DeviceHealth::Ok,
        };
        let amd = parse_amd_json(RAPHAEL_JSON).remove(0);
        assert_ne!(
            nvidia.key(),
            amd.key(),
            "an index-only identity makes a hold on one free the other"
        );

        // …and a genuine duplicate from one probe still collapses, because
        // `cap.gpus` is what admission filters on.
        let mut dupes = vec![nvidia.clone(), amd, nvidia];
        dupes.sort_by_key(mesh_proto::Device::key);
        dupes.dedup_by_key(|d| d.key());
        assert_eq!(dupes.len(), 2);
    }

    #[test]
    fn a_bare_gpu_request_does_not_pick_the_integrated_one() {
        // The failure this exists for, from a real desktop: a 10GB RTX 3080
        // and the CPU's 512MB Raphael iGPU, both index 0. `--gpus 1` chose
        // the iGPU because the inventory happened to sort "amd" first, and
        // the job came back with an empty CUDA_VISIBLE_DEVICES after
        // spending ninety seconds installing torch.
        use crate::gangs::Leases;
        let amd = parse_amd_json(RAPHAEL_JSON).remove(0);
        let nvidia = Device {
            index: 0,
            kind: DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: "NVIDIA GeForce RTX 3080".into(),
            uuid: "GPU-ff6a780d".into(),
            mem_mb: 10_240,
            numa_node: None,
            health: DeviceHealth::Ok,
        };
        // Inventory order must not decide the answer, so assert both ways.
        for inventory in [
            vec![amd.clone(), nvidia.clone()],
            vec![nvidia.clone(), amd.clone()],
        ] {
            let leases = Leases::default();
            let req = mesh_proto::DeviceRequest {
                count: 1,
                ..Default::default()
            };
            let free = leases.free_devices(&inventory, &req, 0, |_| None);
            assert_eq!(
                free.first().map(|d| d.model.as_str()),
                Some("NVIDIA GeForce RTX 3080"),
                "a bare --gpus 1 must not hand out the display adapter"
            );
        }
    }

    #[test]
    fn a_held_device_is_looked_up_by_identity_not_by_ordinal() {
        // The other half of the same failure. Allocation leased the right
        // card; the *lookup* afterwards matched on the bare ordinal, found
        // the iGPU that shares it, and masked the job for the wrong vendor.
        use crate::gangs::{Holder, Leases};
        let amd = parse_amd_json(RAPHAEL_JSON).remove(0);
        let nvidia = Device {
            index: 0,
            kind: DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: "NVIDIA GeForce RTX 3080".into(),
            uuid: "GPU-ff6a780d".into(),
            mem_mb: 10_240,
            numa_node: None,
            health: DeviceHealth::Ok,
        };
        let inventory = [amd, nvidia.clone()];
        let mut leases = Leases::default();
        let holder = Holder::Job(mesh_proto::JobId::generate());
        leases.hold_devices(holder, &[&nvidia], false, None);

        let held = leases.devices_of(&holder);
        assert_eq!(held.len(), 1);
        let found: Vec<&Device> = held
            .iter()
            .filter_map(|k| inventory.iter().find(|d| &d.key() == k))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].vendor, "nvidia", "the ordinal alone is ambiguous");
        // And the mask that follows from it names the card it was given.
        let env = mesh_proto::allocation_env(&[found[0].clone()]);
        let cuda = env
            .iter()
            .find(|(k, _)| k == "CUDA_VISIBLE_DEVICES")
            .map(|(_, v)| v.as_str());
        assert_eq!(cuda, Some("0"));
    }

    #[test]
    fn each_vendor_mask_lists_only_its_own_devices() {
        let nvidia = Device {
            index: 0,
            kind: DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: "NVIDIA GeForce RTX 3080".into(),
            uuid: "GPU-ff6a780d".into(),
            mem_mb: 10240,
            numa_node: None,
            health: DeviceHealth::Ok,
        };
        let env = mesh_proto::allocation_env(&[nvidia]);
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .expect("the allocation sets every vendor variable")
        };
        assert_eq!(get("CUDA_VISIBLE_DEVICES"), "0");
        // The job was given the NVIDIA card. Pointing HIP at "0" as well
        // would hand a HIP task the AMD iGPU that shares the ordinal — and
        // leaving it unset would show it every device on the box.
        assert_eq!(get("HIP_VISIBLE_DEVICES"), "");
        assert_eq!(get("ROCR_VISIBLE_DEVICES"), "");
    }

    #[test]
    fn bus_ids_normalise_to_the_sysfs_spelling() {
        assert_eq!(
            normalise_bdf("00000000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            normalise_bdf("0000:65:00.0").as_deref(),
            Some("0000:65:00.0")
        );
        assert_eq!(normalise_bdf("").as_deref(), None);
        assert_eq!(normalise_bdf("N/A").as_deref(), None);
    }

    #[test]
    fn only_uncorrected_ecc_takes_a_card_out_of_service() {
        assert_eq!(nvidia_health("0"), DeviceHealth::Ok);
        // Unknown — no ECC on the part, or a driver that will not say — is
        // neutral, like every other unknown in the capacity model.
        assert_eq!(nvidia_health("N/A"), DeviceHealth::Ok);
        assert!(matches!(
            nvidia_health("3"),
            DeviceHealth::Unavailable { .. }
        ));
        assert!(!nvidia_health("3").schedulable());
    }

    #[test]
    fn a_missing_probe_is_none_rather_than_a_failure() {
        assert!(run("meshware-no-such-probe-binary", &["--help"]).is_none());
    }
}
