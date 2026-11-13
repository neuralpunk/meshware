//! An upgrade that does not start is undone (ENG-12).
//!
//! `upgrade --self` renamed the new binary over the only copy of the old
//! one, without an fsync, having checked no more than that `--version`
//! printed the target. A release that crashed at boot took every node it
//! reached down with it, and getting one back meant SSH and copying a
//! binary by hand.
//!
//! Now the swap keeps the binary it replaces as `<exe>.prev`, makes both
//! renames durable, and leaves a *pending* marker in the data directory.
//! Each `meshnode up` counts itself against that marker before it does
//! anything else. The daemon clears it once it has been up and healthy
//! for [`HEALTHY_FOR`]; a binary that has started [`MAX_STARTS`] times
//! without getting that far is replaced by `<exe>.prev`, which is then
//! started in its place. `meshnode upgrade --rollback` does the same by
//! hand, at any time.
//!
//! The files, all small and all in the data directory:
//! - `upgrade-pending.json` — an upgrade not yet confirmed;
//! - `upgrade-rolled-back.json` — the last automatic rollback, for the
//!   operator and for a rollout driver that asks this node again.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Starts a new binary gets to reach [`HEALTHY_FOR`] before it is replaced.
///
/// Well under the unit's `StartLimitBurst=10`: a rollback that systemd has
/// already given up on restarting would never run.
pub const MAX_STARTS: u32 = 3;

/// How long a new binary must run healthy before its upgrade is confirmed.
pub const HEALTHY_FOR: Duration = Duration::from_secs(60);

const PENDING: &str = "upgrade-pending.json";
const ROLLED_BACK: &str = "upgrade-rolled-back.json";

/// An upgrade that has not yet proved itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The version that was replaced, and is kept at [`Self::prev`].
    pub from: String,
    /// The version installed at [`Self::exe`].
    pub to: String,
    pub exe: PathBuf,
    pub prev: PathBuf,
    /// Starts of `to` so far.
    pub starts: u32,
    pub installed_at_micros: u64,
}

/// An automatic rollback, as the node that did it recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    /// The version that failed to start.
    pub failed: String,
    /// The version put back.
    pub restored: String,
    pub starts: u32,
    pub at_micros: u64,
    /// Where the binary that failed was kept.
    pub failed_exe: PathBuf,
}

/// `<exe>.prev`: where the binary an upgrade replaced is kept.
pub fn prev_path(exe: &Path) -> PathBuf {
    sibling(exe, "prev")
}

/// `<exe>.failed`: where a rolled-back binary is kept, for whoever
/// investigates.
pub fn failed_path(exe: &Path) -> PathBuf {
    sibling(exe, "failed")
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "meshnode".into());
    exe.with_file_name(format!("{name}.{suffix}"))
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Make `dest` a durable copy of `src` — a hard link when the filesystem
/// allows one, which is free and is the same bytes.
fn keep_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    let tmp = sibling(dest, &format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(src, &tmp).is_err() {
        std::fs::copy(src, &tmp)?;
        std::fs::File::open(&tmp)?.sync_all()?;
    }
    std::fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Put the verified, staged binary at `exe`, keeping what was there as
/// `<exe>.prev`. Both renames are made durable before this returns.
///
/// `staged` must be on the same filesystem as `exe`.
pub fn install(staged: &Path, exe: &Path) -> std::io::Result<PathBuf> {
    std::fs::File::open(staged)?.sync_all()?;
    let prev = prev_path(exe);
    keep_copy(exe, &prev)?;
    std::fs::rename(staged, exe)?;
    sync_dir(parent(exe))?;
    Ok(prev)
}

/// Put `<exe>.prev` back at `exe`. The binary it replaces is kept as
/// `<exe>.failed`; `.prev` is consumed.
pub fn restore_prev(exe: &Path) -> std::io::Result<()> {
    let prev = prev_path(exe);
    if !prev.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no previous binary kept at {}", prev.display()),
        ));
    }
    // Best effort: losing the evidence must not stop the rollback.
    let _ = keep_copy(exe, &failed_path(exe));
    std::fs::rename(&prev, exe)?;
    sync_dir(parent(exe))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let tmp = sibling(path, "tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    sync_dir(parent(path))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn pending(data_dir: &Path) -> Option<Pending> {
    read_json(&data_dir.join(PENDING))
}

pub fn set_pending(data_dir: &Path, pending: &Pending) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    write_json(&data_dir.join(PENDING), pending)
}

pub fn clear_pending(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data_dir.join(PENDING)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub fn rolled_back(data_dir: &Path) -> Option<RolledBack> {
    read_json(&data_dir.join(ROLLED_BACK))
}

/// Is `pending` about the binary this process was started from?
fn is_this_binary(pending: &Pending, version: &str, exe: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    pending.to == version && canonical(&pending.exe) == canonical(exe)
}

/// What [`on_start`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartVerdict {
    /// No upgrade is waiting on this start.
    Nothing,
    /// This start was counted: `starts` of [`MAX_STARTS`].
    Counted { starts: u32 },
    /// The marker was about some other binary, and was dropped.
    Stale,
    /// Too many starts: the previous binary is back at `exe`, and this
    /// process should hand over to it.
    RolledBack(RolledBack),
    /// Too many starts, and the previous binary could not be put back.
    RollbackFailed(String),
}

/// Count this start of `version`, run from `exe`, against a pending
/// upgrade — the first thing `meshnode up` does.
pub fn on_start(data_dir: &Path, version: &str, exe: &Path) -> StartVerdict {
    let Some(mut pending) = pending(data_dir) else {
        return StartVerdict::Nothing;
    };
    if !is_this_binary(&pending, version, exe) {
        // Somebody put a different binary in place — a rollback by hand, a
        // package upgrade. Whatever this marker was waiting for is not
        // what is running.
        let _ = clear_pending(data_dir);
        return StartVerdict::Stale;
    }
    pending.starts += 1;
    if pending.starts <= MAX_STARTS {
        return match set_pending(data_dir, &pending) {
            Ok(()) => StartVerdict::Counted {
                starts: pending.starts,
            },
            // A start that cannot be counted is not held against the
            // binary: this is the disk failing, not the release.
            Err(_) => StartVerdict::Nothing,
        };
    }
    roll_back(
        data_dir,
        pending,
        "has started too many times without becoming healthy",
    )
}

/// Roll a pending upgrade of `version` back *now*: this binary cannot
/// start at all — it refuses the configuration the previous one was
/// running with, and exits with a status the service manager does not
/// retry, so no further start would ever be counted.
pub fn abandon(data_dir: &Path, version: &str, exe: &Path) -> StartVerdict {
    match pending(data_dir) {
        Some(p) if is_this_binary(&p, version, exe) => roll_back(
            data_dir,
            p,
            "refuses the configuration the previous version was running with",
        ),
        _ => StartVerdict::Nothing,
    }
}

fn roll_back(data_dir: &Path, pending: Pending, why: &str) -> StartVerdict {
    match restore_prev(&pending.exe) {
        Ok(()) => {
            let record = RolledBack {
                failed: pending.to.clone(),
                restored: pending.from.clone(),
                starts: pending.starts.min(MAX_STARTS),
                at_micros: mesh_proto::now_micros(),
                failed_exe: failed_path(&pending.exe),
            };
            let _ = write_json(&data_dir.join(ROLLED_BACK), &record);
            let _ = clear_pending(data_dir);
            StartVerdict::RolledBack(record)
        }
        Err(e) => {
            // Leave the marker: the count keeps saying what happened, and
            // a later start with the directory fixed tries again.
            let _ = set_pending(data_dir, &pending);
            StartVerdict::RollbackFailed(format!(
                "meshnode {} {why}, and {} could not be put back: {e}",
                pending.to,
                pending.prev.display()
            ))
        }
    }
}

/// Confirm a pending upgrade of `version` at `exe`: it has been healthy
/// long enough. Returns the confirmed marker, if there was one.
pub fn confirm(data_dir: &Path, version: &str, exe: &Path) -> std::io::Result<Option<Pending>> {
    match pending(data_dir) {
        Some(p) if is_this_binary(&p, version, exe) => {
            clear_pending(data_dir)?;
            Ok(Some(p))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn exe_in(dir: &Path, body: &str) -> PathBuf {
        let exe = dir.join("meshnode");
        std::fs::write(&exe, body).unwrap();
        exe
    }

    fn pending_for(exe: &Path) -> Pending {
        Pending {
            from: "1.0.0".into(),
            to: "1.1.0".into(),
            exe: exe.to_path_buf(),
            prev: prev_path(exe),
            starts: 0,
            installed_at_micros: 1,
        }
    }

    #[test]
    fn install_keeps_the_old_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "old");
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "new").unwrap();
        let prev = install(&staged, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "old");
        assert!(!staged.exists());

        // A second upgrade keeps the one before it, not the first.
        let staged = dir.path().join("staged");
        std::fs::write(&staged, "newer").unwrap();
        install(&staged, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "new");

        restore_prev(&exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(failed_path(&exe)).unwrap(), "newer");
        assert!(!prev.exists());
        assert!(restore_prev(&exe).is_err(), "nothing left to restore");
    }

    #[test]
    fn a_binary_that_never_becomes_healthy_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "new");
        std::fs::write(prev_path(&exe), "old").unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        set_pending(&data, &pending_for(&exe)).unwrap();

        for n in 1..=MAX_STARTS {
            assert_eq!(
                on_start(&data, "1.1.0", &exe),
                StartVerdict::Counted { starts: n }
            );
        }
        let StartVerdict::RolledBack(record) = on_start(&data, "1.1.0", &exe) else {
            panic!("not rolled back");
        };
        assert_eq!(record.starts, MAX_STARTS);
        assert_eq!(record.failed, "1.1.0");
        assert_eq!(record.restored, "1.0.0");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert_eq!(rolled_back(&data), Some(record));
        assert!(pending(&data).is_none());
        // The restored binary starts with nothing to count.
        assert_eq!(on_start(&data, "1.0.0", &exe), StartVerdict::Nothing);
    }

    #[test]
    fn a_healthy_binary_is_confirmed_and_counts_no_further() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "new");
        let data = dir.path().to_path_buf();
        set_pending(&data, &pending_for(&exe)).unwrap();
        assert_eq!(
            on_start(&data, "1.1.0", &exe),
            StartVerdict::Counted { starts: 1 }
        );
        // Another version's daemon does not confirm it.
        assert_eq!(confirm(&data, "1.0.0", &exe).unwrap(), None);
        assert!(confirm(&data, "1.1.0", &exe).unwrap().is_some());
        for _ in 0..=MAX_STARTS {
            assert_eq!(on_start(&data, "1.1.0", &exe), StartVerdict::Nothing);
        }
    }

    #[test]
    fn a_binary_that_refuses_the_config_is_replaced_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "new");
        std::fs::write(prev_path(&exe), "old").unwrap();
        let data = dir.path().to_path_buf();
        // Nothing pending: nothing to abandon.
        assert_eq!(abandon(&data, "1.1.0", &exe), StartVerdict::Nothing);
        set_pending(&data, &pending_for(&exe)).unwrap();
        assert_eq!(
            on_start(&data, "1.1.0", &exe),
            StartVerdict::Counted { starts: 1 }
        );
        let StartVerdict::RolledBack(record) = abandon(&data, "1.1.0", &exe) else {
            panic!("not rolled back");
        };
        assert_eq!(record.starts, 1);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn a_marker_about_another_binary_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "new");
        let data = dir.path().to_path_buf();
        set_pending(&data, &pending_for(&exe)).unwrap();
        // Rolled back by hand: the old version is what starts.
        assert_eq!(on_start(&data, "1.0.0", &exe), StartVerdict::Stale);
        assert!(pending(&data).is_none());

        // A different copy of the binary — a demo, a test — is not this one.
        set_pending(&data, &pending_for(&exe)).unwrap();
        let other = dir.path().join("elsewhere");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(on_start(&data, "1.1.0", &other), StartVerdict::Stale);
    }

    #[test]
    fn a_rollback_with_nothing_to_restore_says_so_and_keeps_counting() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe_in(dir.path(), "new");
        let data = dir.path().to_path_buf();
        let mut p = pending_for(&exe);
        p.starts = MAX_STARTS;
        set_pending(&data, &p).unwrap();
        let StartVerdict::RollbackFailed(why) = on_start(&data, "1.1.0", &exe) else {
            panic!("expected a failure");
        };
        assert!(why.contains("could not be put back"), "{why}");
        assert!(why.contains("too many times"), "{why}");
        assert_eq!(pending(&data).unwrap().starts, MAX_STARTS + 1);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
    }
}
