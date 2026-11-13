//! This machine's node, kept at the `meshnode` Director carries.
//!
//! When Director ran a node inside itself, that node was always Director's
//! own version: updating Director updated it, and every screen that asks
//! "which nodes are behind" was measured against it. The fleet rollouts and
//! the SSH updates left it out, rightly — it is the machine Director is
//! running on.
//!
//! The node is `meshnode` now, a service of its own, and it would otherwise
//! stay at whatever version it was first installed at: counted as behind by
//! nothing (the newest version *running* is its own), and offered to the SSH
//! update as a machine to log into as root — this one. So Director keeps
//! the old promise the new way. When the `meshnode` it carries is newer
//! than this machine's node, Director installs that binary where the unit
//! runs it from and restarts the unit, once, when it connects — which is
//! what installing a new Director has always meant for this machine. It
//! never moves a node backwards, and it leaves alone a node it did not
//! start: one run by hand in a terminal, or a packaged system service.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mesh_proto::{IpcRequest, IpcResponse};

use crate::engine::{self, Engine};

/// The `meshnode` shipped inside this Director, and its version.
#[derive(Debug, Clone)]
pub struct Carried {
    pub path: PathBuf,
    pub version: String,
}

/// Where the bundled `meshnode` binaries are, under Tauri's resource
/// directory.
///
/// **`resources/meshnode`, not `meshnode`.** `tauri.conf.json` lists them as
/// `resources/meshnode/*`, and Tauri keeps that relative path: a packaged
/// install puts them at `<resource dir>/resources/meshnode/`, and a binary
/// run from `target/` — whose resource directory is its own — finds them at
/// `target/<profile>/resources/meshnode/`. Every caller used to look one
/// level up, found nothing, and fell through to its next source without a
/// word: Add machines and Update over SSH never used the binary Director
/// shipped with, and this machine's node was never kept at it. The old
/// spelling is still tried second, for a layout that flattened it.
pub fn bundle_dir(resource_dir: Option<&Path>) -> Option<PathBuf> {
    let rd = resource_dir?;
    [rd.join("resources").join("meshnode"), rd.join("meshnode")]
        .into_iter()
        .find(|d| d.is_dir())
}

/// What this Director carries: `resources/meshnode/meshnode-<arch>-…`,
/// versioned by the `VERSION` file `ship.sh` writes beside it, or by asking
/// the binary when there is none.
pub fn carried(resource_dir: Option<&Path>) -> Option<Carried> {
    let dir = bundle_dir(resource_dir)?;
    let path = dir.join(format!(
        "meshnode-{}-unknown-linux-musl",
        std::env::consts::ARCH
    ));
    if !path.is_file() {
        return None;
    }
    let version = std::fs::read_to_string(dir.join("VERSION"))
        .ok()
        .map(|v| v.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            let out = std::process::Command::new(&path)
                .arg("--version")
                .output()
                .ok()?;
            crate::deploy::version_from_output(&out.stdout)
        })?;
    Some(Carried { path, version })
}

/// Bring this machine's node up to `carried`, if it is older and Director
/// runs it. `Some(version)` when it was updated; `None` when there was
/// nothing to do, or nothing Director should do — each case logged, and
/// none of them a reason to fail the connection that asked.
pub async fn keep_current(engine: &Engine, carried: Option<Carried>) -> Option<String> {
    if engine::demo_root().is_some() {
        return None;
    }
    let carried = carried?;
    let running = match engine.ipc(IpcRequest::Status).await {
        Ok(IpcResponse::Status(info)) => info.version,
        _ => return None,
    };
    if mesh_proto::version_cmp(&carried.version, &running).is_le() {
        return None;
    }
    // Only the node the user unit runs, in the home the unit runs it in.
    // One started by hand in a terminal is somebody's on purpose, and
    // restarting the unit under it would only fail on the op-log's lock.
    let ours = mesh_core::usernode::home().is_some_and(|h| same_dir(&h, &engine::node_home()))
        && mesh_core::usernode::installed()
        && mesh_core::usernode::active();
    if !ours {
        tracing::info!(
            running = %running,
            carried = %carried.version,
            "this machine's node is older than the meshnode Director carries, and is not \
             the user service Director updates; leaving it alone"
        );
        return None;
    }
    tracing::info!(from = %running, to = %carried.version, "updating this machine's node");
    let path = carried.path.clone();
    let installed = tokio::task::spawn_blocking(move || {
        mesh_core::usernode::install(&path)?;
        mesh_core::usernode::restart()
    })
    .await;
    match installed {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!("updating this machine's node failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::warn!("updating this machine's node panicked");
            return None;
        }
    }
    // Back, and on the new version, before anyone reads a status from it.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(IpcResponse::Status(info)) = engine.ipc(IpcRequest::Status).await
            && info.version == carried.version
        {
            return Some(carried.version);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tracing::warn!("this machine's node was updated but has not come back yet");
    None
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_director_carries_is_read_from_its_version_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(carried(Some(dir.path())).is_none(), "nothing bundled");
        // Where Tauri actually puts it: the relative path from tauri.conf.
        let res = dir.path().join("resources").join("meshnode");
        std::fs::create_dir_all(&res).expect("mkdir");
        let bin = res.join(format!(
            "meshnode-{}-unknown-linux-musl",
            std::env::consts::ARCH
        ));
        std::fs::write(&bin, b"not really a binary").expect("write");
        std::fs::write(res.join("VERSION"), "v0.9.30\n").expect("write");
        let c = carried(Some(dir.path())).expect("carried");
        assert_eq!(c.version, "0.9.30");
        assert_eq!(c.path, bin);
    }
}
