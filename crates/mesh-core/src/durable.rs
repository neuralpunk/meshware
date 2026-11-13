//! Writing a file so that a power cut leaves either the old contents or
//! the new ones, and never half of either (ENG-16).
//!
//! `std::fs::write` truncates and then writes. Between those two steps the
//! file is empty, and after them its bytes are in the page cache and not on
//! the disk. Neither matters for a file that can be regenerated; both
//! matter for a file that *is* the record — a job's `status.json`, a
//! merged archive part — where the failure is not a lost update but a node
//! that comes back up holding a job it can no longer settle, or an archive
//! whose index points into nothing.
//!
//! The sequence here is the usual one, in the order that makes it true:
//!
//! 1. write the new contents to a temporary file beside the target,
//! 2. `fsync` that file, so its bytes are durable before anything points
//!    at them,
//! 3. `rename` it over the target — atomic within a filesystem, so a
//!    reader sees one version or the other,
//! 4. `fsync` the *directory*, so the rename itself survives the crash.
//!
//! Step 4 is the one that gets left out, and leaving it out is what makes
//! the whole thing a comfortable fiction: without it the rename is a
//! metadata change sitting in the same cache the data was in.
//!
//! The temp file is named after the target and carries the writer's pid, so
//! two processes on one data dir cannot collide and a leftover is
//! recognisable. Leftovers are cleaned by the sweeps that own the
//! directory, not here — this function's job is one file.

use std::io::Write as _;
use std::path::Path;

use crate::error::{CoreError, Result};

/// Replace `path` with `bytes`, atomically and durably.
///
/// Returns an error rather than leaving a partial file: every caller here
/// would rather keep the previous version than record an unreadable one.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    write_with_mode(path, bytes, None)
}

/// [`write`], with an explicit Unix mode on the file created.
///
/// The mode is set at `open` rather than after it, for the reason
/// `keyfile::write_private` gives: a create-then-chmod leaves a window in
/// which the file is readable by more of the machine than intended.
pub fn write_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        CoreError::Io(std::io::Error::other(format!(
            "{} has no parent directory",
            path.display()
        )))
    })?;
    std::fs::create_dir_all(dir)?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".into());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));

    // A leftover from a crash mid-write is overwritten, never appended to.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;

    let written = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        // Before the rename, not after: the rename is what publishes these
        // bytes, and publishing bytes that are not on the disk yet is the
        // bug this exists to prevent.
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    // The rename is metadata, and metadata is cached too. A directory that
    // cannot be opened for sync is not a reason to fail a write that has
    // already landed — some filesystems refuse the open — so this is
    // best-effort and deliberately quiet.
    sync_dir(dir);
    Ok(())
}

/// fsync a directory, so a rename or a create inside it is durable.
pub fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_previous_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        write(&path, b"first").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"first");
        write(&path, b"second").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
    }

    /// The whole point: no temp file is left where a reader might mistake
    /// it for state, and the directory holds exactly one entry.
    #[test]
    fn nothing_is_left_beside_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("spec.json"), b"{}").expect("write");
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["spec.json".to_string()], "{entries:?}");
    }

    /// A failed write must not destroy what was already there. The target
    /// being a *directory* is the reachable version of this: the rename
    /// fails, and the previous file — here, the directory — is untouched.
    #[test]
    fn a_failed_write_leaves_no_debris() {
        let dir = tempfile::tempdir().expect("tempdir");
        let occupied = dir.path().join("occupied");
        std::fs::create_dir(&occupied).expect("mkdir");
        std::fs::write(occupied.join("inside"), b"keep me").expect("write");

        assert!(write(&occupied, b"nope").is_err(), "must refuse");
        assert_eq!(
            std::fs::read(occupied.join("inside")).expect("read"),
            b"keep me"
        );
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "left a temp file behind: {stray:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_mode_is_applied_at_creation() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        write_with_mode(&path, b"x", Some(0o600)).expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
}
