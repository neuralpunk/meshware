//! Ephemeral objects: the parallel data plane (MASTERPLAN M9 §5.5).
//!
//! A bundle is durable, replicated where it is needed, and indexed in the
//! op-log. An object is none of those things. It is the *intermediate* a
//! task produces for another task to read: written where it was computed,
//! fetched on demand by whoever needs it, refcounted to the job that made
//! it, and located through a rendezvous-hashed directory that no node
//! coordinates.
//!
//! The reason this class exists at all is G4 in the plan: without it every
//! intermediate byte funnels through the node that accepted the job, and
//! that node becomes exactly the central bottleneck the project exists to
//! avoid. With it, a worker's output stays on the worker and the origin
//! sees a 40-byte reference.
//!
//! Objects are **not replicated** (M9 §10, Q3). Inputs are content-addressed
//! and tasks are idempotent, so recomputing a lost object is cheaper than
//! copying every object that might be lost. The *directory entry* is a
//! different matter — it is a hundred bytes, and losing it would strand
//! bytes that still exist — so entries land on the first
//! [`DIRECTORY_REPLICAS`] nodes of the rendezvous ranking.

use serde::{Deserialize, Serialize};

use crate::{ChunkHash, JobId, NodeId, ObjectId};

/// How many nodes hold the directory entry for an object. The bytes are
/// k=1; only the index is replicated, and only this far.
pub const DIRECTORY_REPLICAS: usize = 2;

/// Rendezvous context for the object directory. Every node computes the
/// same ranking from the same object id, so "who knows where this is?"
/// needs no coordination — the same primitive job adoption already uses
/// (MASTERPLAN §10.3).
pub const OBJDIR_CONTEXT: &[u8] = b"objdir";

/// An object's chunk list. The manifest is what a fetcher asks the holder
/// for; the chunks themselves come back through the ordinary `GetChunk`
/// path, because a chunk hash is a global content address and it does not
/// matter which class of object it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectManifest {
    pub chunks: Vec<ChunkHash>,
    pub size: u64,
}

impl ObjectManifest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).unwrap_or_default()
    }

    /// The ObjectId = BLAKE3 of the canonical encoding, exactly as a
    /// BundleId is the hash of its manifest. Identical content therefore
    /// yields an identical id on every node, which is what makes losing an
    /// object recoverable by re-running the task that made it.
    pub fn object_id(&self) -> ObjectId {
        ObjectId(*blake3::hash(&self.canonical_bytes()).as_bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// What travels in place of the bytes: an item's output, named and sized,
/// with the node that has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub id: ObjectId,
    pub size: u64,
    /// The node that produced it. A hint, not a guarantee — the directory
    /// is authoritative, and this node may since have gone away.
    pub holder: NodeId,
}

/// One object as the local store knows it (`meshnode objects ls`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRow {
    pub id: ObjectId,
    pub job: JobId,
    pub size: u64,
    /// Bytes of this object still resident in RAM; the rest has spilled.
    pub resident: u64,
    pub age_secs: u64,
    /// Held past the ordinary TTL because it is a finished run's result and
    /// nobody has fetched it yet (R3 P3). The difference between "this will
    /// be gone in fifteen minutes" and "this is waiting for you".
    pub awaiting_collection: bool,
}

/// Where an item's output goes (M9 §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Capture {
    /// Inline in the chunk report, bounded. This is `meshnode parallel`:
    /// for `-a hosts.txt`-shaped work the results are small and the origin
    /// absorbs them without becoming a bottleneck (M9 §10, Q1).
    #[default]
    Inline,
    /// Into the producing node's ephemeral object store; only the
    /// reference travels. This is `meshnode map`, and it is the whole
    /// point of the object class.
    Object,
}

impl Capture {
    pub fn is_object(&self) -> bool {
        matches!(self, Capture::Object)
    }
}

/// The file path each item may write its output to. An item that writes
/// nothing there has its stdout captured instead, so an ordinary
/// `grep`-shaped command needs no modification to take part.
pub const OUT_ENV: &str = "MESHWARE_OUT";

/// Directory a reduce step finds its inputs in, one file per map output.
pub const INPUTS_ENV: &str = "MESHWARE_INPUTS";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_is_the_same_object_everywhere() {
        let a = ObjectManifest {
            chunks: vec![ChunkHash([1; 32]), ChunkHash([2; 32])],
            size: 99,
        };
        let b = a.clone();
        assert_eq!(a.object_id(), b.object_id());
        // The id covers the size as well as the chunks: a truncated
        // manifest must not masquerade as the whole object.
        let short = ObjectManifest {
            size: 98,
            ..a.clone()
        };
        assert_ne!(a.object_id(), short.object_id());
    }

    #[test]
    fn object_ids_round_trip_through_their_display_form() {
        let id = ObjectManifest {
            chunks: vec![ChunkHash([7; 32])],
            size: 3,
        }
        .object_id();
        let shown = id.to_display_string();
        assert!(shown.starts_with("obj-"));
        assert_eq!(ObjectId::parse(&shown).expect("parse"), id);
        assert!(ObjectId::parse("bndl-00").is_err());
    }
}
