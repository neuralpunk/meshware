//! Live resource use of running instances (Phase B, E3).
//!
//! Measured on demand, on the node that runs the work, and answered to
//! whoever asked — never replicated (see [`mesh_proto::InstanceStat`]).
//! Three sources, best first, and the answer says which it used:
//!
//! - **container** — the engine's own `stats`. A container's processes are
//!   conmon's children, in neither this node's cgroup nor its process tree,
//!   so nothing else here can see them;
//! - **cgroup** — the job's own cgroup, which counts every process the job
//!   forked;
//! - **process** — `/proc` for the job's main process alone. A lower bound,
//!   and labelled as one, because a script that forks its real work reads as
//!   an idle shell here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mesh_proto::{InstanceStat, JobId};

/// How long a CPU reading is averaged over. Short enough to answer an IPC
/// call promptly, long enough that one scheduler tick does not read as 0 %
/// or 100 %.
pub const CPU_WINDOW: Duration = Duration::from_millis(250);

/// `USER_HZ`: the unit `/proc/<pid>/stat` counts CPU time in. Fixed at 100
/// for userspace on every architecture Linux ships, whatever the kernel's
/// own tick rate — which is why it can be a constant rather than a query.
const USER_HZ: u64 = 100;

/// Measure one job this node runs.
///
/// `engine` is set for a container instance, `cgroup` is the job's cgroup
/// directory where this node enforces cgroups, and `pid` its main process.
/// Whatever cannot be measured stays `None`; nothing is guessed.
pub async fn measure(
    job: JobId,
    engine: Option<crate::engines::Engine>,
    cgroup: Option<PathBuf>,
    pid: Option<u32>,
) -> InstanceStat {
    let mut stat = InstanceStat {
        job,
        cpu_pct: None,
        mem_bytes: None,
        mem_limit_bytes: None,
        source: String::new(),
    };
    if let Some(engine) = engine {
        if let Some((cpu, mem, limit)) = container_stats(&engine, &job.short()).await {
            stat.cpu_pct = cpu;
            stat.mem_bytes = mem;
            stat.mem_limit_bytes = limit;
            stat.source = "container".into();
        }
        // A container's own processes are nowhere else to be found, so an
        // engine that would not answer is an honest "unmeasured".
        return stat;
    }
    if let Some(dir) = cgroup.filter(|d| d.join("cpu.stat").exists()) {
        let before = read_cgroup(&dir);
        tokio::time::sleep(CPU_WINDOW).await;
        if let (Some((c1, _, _)), Some((c2, mem, limit))) = (before, read_cgroup(&dir)) {
            stat.cpu_pct = Some(percent(c1, c2, CPU_WINDOW));
            stat.mem_bytes = mem;
            stat.mem_limit_bytes = limit;
            stat.source = "cgroup".into();
            return stat;
        }
    }
    if let Some(pid) = pid {
        let proc_root = Path::new("/proc");
        let before = read_process(proc_root, pid);
        tokio::time::sleep(CPU_WINDOW).await;
        if let (Some((c1, _)), Some((c2, rss))) = (before, read_process(proc_root, pid)) {
            stat.cpu_pct = Some(percent(c1, c2, CPU_WINDOW));
            stat.mem_bytes = rss;
            stat.source = "process".into();
        }
    }
    stat
}

/// Percent of one core used between two cumulative readings in microseconds.
fn percent(before_micros: u64, after_micros: u64, window: Duration) -> f32 {
    let used = after_micros.saturating_sub(before_micros) as f64;
    let span = window.as_micros().max(1) as f64;
    (used / span * 100.0) as f32
}

/// A cgroup's cumulative CPU time in microseconds, its current memory, and
/// its memory ceiling unless that is `max`.
pub fn read_cgroup(dir: &Path) -> Option<(u64, Option<u64>, Option<u64>)> {
    let cpu = std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse::<u64>().ok())?;
    Some((
        cpu,
        read_u64(&dir.join("memory.current")),
        read_u64(&dir.join("memory.max")),
    ))
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// One process's cumulative CPU time in microseconds, and its resident
/// memory, from `<proc_root>/<pid>/stat` and `status`.
pub fn read_process(proc_root: &Path, pid: u32) -> Option<(u64, Option<u64>)> {
    let dir = proc_root.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // The command name is parenthesised and may itself contain spaces and
    // parentheses, so the fields are counted from the *last* `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line: 12th and 13th
    // after the name, counting `state` as the first.
    let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let rss = std::fs::read_to_string(dir.join("status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .map(|kib| kib.saturating_mul(1024));
    Some((ticks.saturating_mul(1_000_000) / USER_HZ, rss))
}

/// Ask this node's container engine what one instance's container uses.
async fn container_stats(
    engine: &crate::engines::Engine,
    job_short: &str,
) -> Option<(Option<f32>, Option<u64>, Option<u64>)> {
    let name = crate::engines::container_name(job_short);
    let run = tokio::process::Command::new(&engine.program)
        .args(engine.global_args())
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.CPUPerc}}|{{.MemUsage}}",
            name.as_str(),
        ])
        .envs(crate::engines::engine_env())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains('|'))
        .and_then(parse_engine_stats)
}

/// One line of `stats --format '{{.CPUPerc}}|{{.MemUsage}}'`: docker's
/// `1.23%|45.6MiB / 128MiB`, or podman's `1.23%|45.6MB / 128MB`.
pub fn parse_engine_stats(line: &str) -> Option<(Option<f32>, Option<u64>, Option<u64>)> {
    let (cpu, mem) = line.trim().split_once('|')?;
    let cpu = cpu.trim().trim_end_matches('%').trim().parse::<f32>().ok();
    let (used, limit) = match mem.split_once('/') {
        Some((used, limit)) => (parse_size(used), parse_size(limit)),
        None => (parse_size(mem), None),
    };
    Some((cpu, used, limit))
}

/// `45.6MiB`, `128MB`, `1.2GB`, `512kB`, `0B` — both engines' spellings.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let at = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = s.split_at(at);
    let n: f64 = number.trim().parse().ok()?;
    let scale: f64 = match unit.trim() {
        "B" | "b" => 1.0,
        "kB" | "KB" | "k" => 1e3,
        "KiB" => 1024.0,
        "MB" => 1e6,
        "MiB" => 1_048_576.0,
        "GB" => 1e9,
        "GiB" => 1_073_741_824.0,
        "TB" => 1e12,
        "TiB" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some((n * scale) as u64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn both_engines_stats_lines_parse() {
        let docker = parse_engine_stats("0.52%|45.6MiB / 128MiB").unwrap();
        assert_eq!(docker.0, Some(0.52));
        assert_eq!(docker.1, Some(47_815_065));
        assert_eq!(docker.2, Some(134_217_728));
        let podman = parse_engine_stats("12.00%|3.2MB / 128MB\n").unwrap();
        assert_eq!(podman.0, Some(12.0));
        assert_eq!(podman.1, Some(3_200_000));
        assert_eq!(podman.2, Some(128_000_000));
        assert_eq!(parse_engine_stats("no bar here"), None);
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("512kB"), Some(512_000));
        assert_eq!(parse_size("banana"), None);
    }

    /// The process source reads a real `/proc` entry: this test's own.
    #[test]
    fn a_process_reads_its_own_cpu_and_memory() {
        let (cpu, rss) = read_process(Path::new("/proc"), std::process::id()).unwrap();
        assert!(cpu < u64::MAX);
        assert!(rss.unwrap() > 0, "a running test has resident memory");
    }

    /// A name with spaces and a parenthesis must not shift the fields.
    #[test]
    fn a_process_name_with_parentheses_does_not_shift_the_fields() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("42");
        std::fs::create_dir_all(&dir).unwrap();
        // 3 state, 4 ppid ... 14 utime = 250, 15 stime = 50.
        std::fs::write(
            dir.join("stat"),
            "42 (my (odd) name) S 1 42 42 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 1 1 1",
        )
        .unwrap();
        std::fs::write(dir.join("status"), "Name:\tx\nVmRSS:\t   2048 kB\n").unwrap();
        let (cpu, rss) = read_process(root.path(), 42).unwrap();
        assert_eq!(cpu, 3_000_000, "300 ticks at USER_HZ 100 is three seconds");
        assert_eq!(rss, Some(2_097_152));
    }

    #[test]
    fn a_cgroup_reads_usage_and_treats_max_as_no_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cpu.stat"),
            "usage_usec 1500000\nuser_usec 1\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("memory.current"), "1048576\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        assert_eq!(
            read_cgroup(dir.path()),
            Some((1_500_000, Some(1_048_576), None))
        );
        assert!((percent(0, 125_000, CPU_WINDOW) - 50.0).abs() < 0.01);
    }
}
