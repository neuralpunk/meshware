//! Leaving a mesh its founder has reset (`meshnode reset`), on disk.
//!
//! The replicated half is [`mesh_proto::OpBody::MeshReset`]: founder-signed,
//! checked by every node against its own genesis. This is the half each
//! node does to itself once it has stopped: move what belonged to the mesh
//! out of the way, keep what belongs to the machine, and wait.
//!
//! ## Moved, never deleted
//!
//! The op-log, jobs, content and genesis go into `<home>/reset/<when>/`.
//! A reset is the founder's decision about the mesh; what is on a machine's
//! disk is still that machine's owner's, and a tool that erased it would be
//! making a second decision nobody signed. `rm -rf <home>/reset` is a
//! person's to type.
//!
//! What stays is what makes the machine *adoptable*: the binary, the
//! service unit, `meshnode.toml`, the node and user keys, the interpreter
//! cache, logs, and Director's own files. Re-adopting a reset node is a
//! fresh adoption with nothing to install.
//!
//! ## Waiting, not founding
//!
//! A home with no genesis is one `meshnode up` founds a mesh in, and every
//! service unit restarts a node that stops. So a reset node would found a
//! one-node mesh of its own seconds after leaving, and then refuse to be
//! adopted because it already belongs to one. The marker, `reset.json`, is
//! what stops that: while it is there and genesis is not, the node refuses
//! to start and says why. Joining a mesh writes a genesis and makes the
//! marker stale; founding one on purpose — Director's wizard,
//! `meshnode up --new-mesh` — clears it first.
//!
//! ## Interrupted half-way
//!
//! The marker is written first and genesis is moved last, so the marker is
//! also the record of an unfinished reset: a home with a marker and the
//! *same* mesh's genesis is one whose move was cut off, and [`resume`]
//! finishes it before anything opens the op-log. A home with a marker and a
//! *different* mesh's genesis joined a new mesh since, and the marker is
//! removed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::layout::Layout;
use mesh_proto::{MeshId, MeshReset};

/// `<home>/reset.json`: this home left a mesh because its founder reset it.
const MARKER: &str = "reset.json";

/// `<home>/reset/`: one directory per reset, holding what it moved.
const ASIDE_ROOT: &str = "reset";

/// Why a node has stopped and what it is waiting for: the marker's body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub mesh_id: MeshId,
    pub mesh_name: String,
    /// The signed order, kept as the evidence for why this machine left.
    pub reset: MeshReset,
    /// Where the old mesh's data went.
    pub aside: PathBuf,
    /// When this node acted on it.
    pub left_at_micros: u64,
}

impl Marker {
    /// The refusal a reset home gives anything that would start its node.
    pub fn awaiting(&self) -> String {
        format!(
            "this node left mesh '{}' when its founder reset it, and is waiting to be adopted \
             into a new one.\n\n\
             Its old mesh data is in {}.\n\n\
             Adopt it from the new mesh — Director, or `meshnode adopt <host>` there — or join \
             one from here with `meshnode join`. To start a new mesh on this machine instead:\n    \
             meshnode up --new-mesh",
            self.mesh_name,
            self.aside.display()
        )
    }
}

/// Everything in a home that belongs to the mesh rather than to the
/// machine, in the order it is moved. Genesis — `mesh/` — last, so a home
/// is out of the mesh only once everything else has gone.
fn mesh_scoped(layout: &Layout) -> Vec<PathBuf> {
    vec![
        layout.state_dir(),
        layout.jobs_dir(),
        layout.content_dir(),
        layout.objects_dir(),
        layout.git_sources_dir(),
        layout.sessions_dir(),
        layout.sinks_dir(),
        layout.archive_dir(),
        layout.mesh_dir(),
    ]
}

fn marker_path(layout: &Layout) -> PathBuf {
    layout.root().join(MARKER)
}

/// The marker, if this home has one.
///
/// A marker that does not parse is an error rather than an absence: it
/// means somebody's reset is recorded here in a shape this build cannot
/// read, and treating that as "no reset" is how a node would go on to
/// found a mesh of its own.
pub fn marker(layout: &Layout) -> Result<Option<Marker>> {
    let path = marker_path(layout);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))
}

/// Take this home out of `genesis`'s mesh: record the reset, then move the
/// mesh's data aside. The node must have stopped — nothing may write to the
/// op-log while it moves.
pub fn leave(layout: &Layout, genesis: &Genesis, reset: &MeshReset) -> Result<Marker> {
    let marker = Marker {
        mesh_id: genesis.body.mesh_id,
        mesh_name: genesis.body.mesh_name.clone(),
        reset: reset.clone(),
        aside: fresh_aside(layout),
        left_at_micros: mesh_proto::now_micros(),
    };
    write_marker(layout, &marker)?;
    finish(layout, &marker)?;
    Ok(marker)
}

/// Finish an interrupted reset, clear a stale marker, and say whether this
/// home is waiting to be adopted. Run before anything opens the op-log.
///
/// `Some` is a refusal: the home left a mesh and has joined no other.
pub fn resume(layout: &Layout) -> Result<Option<Marker>> {
    let Some(marker) = marker(layout)? else {
        return Ok(None);
    };
    match Genesis::load(layout.root())? {
        Some(genesis) if genesis.body.mesh_id == marker.mesh_id => {
            tracing::warn!(
                aside = %marker.aside.display(),
                "finishing a mesh reset that was interrupted"
            );
            finish(layout, &marker)?;
            Ok(Some(marker))
        }
        Some(_) => {
            // Joined another mesh since: the marker says nothing true.
            clear(layout)?;
            Ok(None)
        }
        None => Ok(Some(marker)),
    }
}

/// Forget that this home was reset, so that a mesh can be founded here.
/// Only for a person's decision to found one. Returns whether there was
/// a marker to clear.
pub fn clear(layout: &Layout) -> Result<bool> {
    match std::fs::remove_file(marker_path(layout)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Move whatever of the mesh is still in the home into `marker.aside`.
/// Idempotent: a second run moves only what the first did not reach.
fn finish(layout: &Layout, marker: &Marker) -> Result<()> {
    std::fs::create_dir_all(&marker.aside)?;
    for from in mesh_scoped(layout) {
        if std::fs::symlink_metadata(&from).is_err() {
            continue;
        }
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = free_name(&marker.aside.join(name));
        // A rename, so it is instant and cannot half-copy — and a home
        // whose `content/` is a mount on another volume fails here rather
        // than starting a copy of every bundle. That is an error naming the
        // path, and the reset finishes on the next start once it is moved.
        std::fs::rename(&from, &to).map_err(|e| {
            CoreError::Config(format!(
                "moving {} to {}: {e}",
                from.display(),
                to.display()
            ))
        })?;
        tracing::info!(from = %from.display(), to = %to.display(), "moved aside");
    }
    Ok(())
}

fn write_marker(layout: &Layout, marker: &Marker) -> Result<()> {
    let json = serde_json::to_vec_pretty(marker)
        .map_err(|e| CoreError::Config(format!("encode {MARKER}: {e}")))?;
    // Written whole and renamed into place: a marker that exists is one a
    // later start can read.
    let path = marker_path(layout);
    let tmp = path.with_extension("json.tmp");
    crate::keyfile::write_private(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// `<home>/reset/20260919T081203Z`, or with a suffix if a reset in the same
/// second already took that name.
fn fresh_aside(layout: &Layout) -> PathBuf {
    let stamp: String = humantime::format_rfc3339_seconds(std::time::SystemTime::now())
        .to_string()
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    free_name(&layout.root().join(ASIDE_ROOT).join(stamp))
}

/// `path`, or `path.2`, `path.3`… — the first that does not exist.
fn free_name(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_err() {
        return path.to_path_buf();
    }
    let mut n = 2u32;
    loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{n}"));
        let candidate = PathBuf::from(name);
        if std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Keypair;

    fn home_in_a_mesh() -> (tempfile::TempDir, Layout, Genesis, MeshReset) {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("home"));
        layout.ensure().expect("ensure");
        let identity =
            crate::identity::Identity::load_or_generate(layout.root()).expect("identity");
        let founder = Keypair::generate().expect("founder key");
        let genesis =
            Genesis::create("test-mesh", &founder, identity.node.node_id()).expect("genesis");
        genesis.save(layout.root()).expect("save genesis");
        std::fs::write(layout.oplog(), b"op-log").expect("oplog");
        std::fs::write(layout.root().join("meshnode.toml"), b"# config").expect("config");
        let mut reset = MeshReset {
            mesh_id: genesis.body.mesh_id,
            by: founder.user_id(),
            at_micros: mesh_proto::now_micros(),
            signature: Vec::new(),
        };
        reset.signature = founder.sign(&reset.signing_bytes()).to_vec();
        (dir, layout, genesis, reset)
    }

    #[test]
    fn leaving_moves_the_mesh_and_keeps_the_machine() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        let node_key = std::fs::read(layout.identity_dir().join("node.key")).expect("node key");

        let marker = leave(&layout, &genesis, &reset).expect("leave");

        assert!(!layout.genesis_file().exists(), "genesis left the home");
        assert!(!layout.oplog().exists(), "the op-log left the home");
        assert_eq!(
            std::fs::read(marker.aside.join("state").join("oplog.redb")).expect("moved oplog"),
            b"op-log"
        );
        assert!(marker.aside.join("mesh").join("genesis.json").exists());
        assert!(layout.root().join("meshnode.toml").exists(), "config stays");
        assert_eq!(
            std::fs::read(layout.identity_dir().join("node.key")).expect("node key"),
            node_key,
            "the node key stays"
        );
        let read = marker_or_panic(&layout);
        assert_eq!(read.mesh_id, genesis.body.mesh_id);
        assert_eq!(read.reset, reset);
    }

    #[test]
    fn a_reset_home_waits_rather_than_founding() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        leave(&layout, &genesis, &reset).expect("leave");
        let waiting = resume(&layout).expect("resume").expect("waits");
        assert!(waiting.awaiting().contains("test-mesh"));
        assert!(waiting.awaiting().contains("--new-mesh"));
        // Asking again changes nothing and moves nothing.
        assert!(resume(&layout).expect("resume").is_some());
    }

    #[test]
    fn an_interrupted_reset_is_finished_before_anything_opens_the_log() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        // The marker is down and nothing has moved: cut off right after
        // the first write.
        let marker = Marker {
            mesh_id: genesis.body.mesh_id,
            mesh_name: genesis.body.mesh_name.clone(),
            reset,
            aside: fresh_aside(&layout),
            left_at_micros: 1,
        };
        write_marker(&layout, &marker).expect("marker");

        assert!(resume(&layout).expect("resume").is_some(), "still waiting");
        assert!(!layout.genesis_file().exists());
        assert!(!layout.oplog().exists());
        assert!(marker.aside.join("state").join("oplog.redb").exists());
    }

    #[test]
    fn joining_another_mesh_makes_the_marker_stale() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        leave(&layout, &genesis, &reset).expect("leave");
        // A join writes the new mesh's genesis.
        let (founder, node) = (
            Keypair::generate().expect("key"),
            Keypair::generate().expect("key"),
        );
        let other = Genesis::create("next", &founder, node.node_id()).expect("genesis");
        other.save(layout.root()).expect("save");

        assert!(resume(&layout).expect("resume").is_none(), "not waiting");
        assert!(marker(&layout).expect("read").is_none(), "marker gone");
        assert!(layout.genesis_file().exists(), "the new mesh is untouched");
    }

    #[test]
    fn founding_on_purpose_clears_the_marker() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        leave(&layout, &genesis, &reset).expect("leave");
        assert!(clear(&layout).expect("clear"));
        assert!(resume(&layout).expect("resume").is_none());
        assert!(
            !clear(&layout).expect("clear again"),
            "nothing left to clear"
        );
    }

    #[test]
    fn two_resets_never_share_a_directory() {
        let (_dir, layout, genesis, reset) = home_in_a_mesh();
        let first = leave(&layout, &genesis, &reset).expect("leave");
        genesis.save(layout.root()).expect("rejoin");
        let second = leave(&layout, &genesis, &reset).expect("leave again");
        assert_ne!(first.aside, second.aside);
        assert!(first.aside.join("state").exists());
        assert!(second.aside.join("mesh").exists());
    }

    fn marker_or_panic(layout: &Layout) -> Marker {
        marker(layout).expect("read marker").expect("a marker")
    }
}
