//! Replicated secret records (MASTERPLAN §7.3).
//!
//! Envelope encryption: the value is sealed with XChaCha20-Poly1305 under a
//! random data key; the data key is wrapped (X25519 sealed box derived from
//! each node's Ed25519 key) to every node authorized to run matching work.
//! The record replicates everywhere, but only wrap targets can decrypt —
//! and the plaintext is produced *client-side*, so it never even transits
//! the daemon or the wire unencrypted. The crypto lives in mesh-core; these
//! are the wire shapes.

use serde::{Deserialize, Serialize};

use crate::{NodeId, UserId};

/// The data key wrapped for one node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecretWrap {
    pub node_id: NodeId,
    /// Ephemeral X25519 public key for this wrap.
    pub eph_pub: [u8; 32],
    /// XChaCha20-Poly1305 nonce for the wrapped key.
    pub nonce: [u8; 24],
    /// The sealed 32-byte data key (+ AEAD tag).
    pub wrapped_key: Vec<u8>,
}

/// A named, versioned, sealed secret. LWW per name by (version, hlc).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecretRecord {
    pub name: String,
    pub version: u32,
    /// XChaCha20-Poly1305 nonce for the value ciphertext.
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
    /// Per-node wraps. Excluded from the signature so nodes can extend them
    /// for late joiners (rewrap) without invalidating the record.
    pub wraps: Vec<SecretWrap>,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl SecretRecord {
    /// Signature coverage: everything except `signature` and `wraps`.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        unsigned.wraps = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-secret-put-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// Where a vault secret goes and who may use it (SEC-7). One per name,
/// admin-signed, and the newest by its signed time wins.
///
/// A secret with no scope goes to every member that may hold one — not a
/// no-execute node or a quarantined one — and any user
/// who may run a job on a node that holds it can read it. A scope narrows
/// both: the nodes to a selector, and the users to a list of people and
/// roles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecretScope {
    pub name: String,
    /// The nodes it is sealed and healed for, in `meshnode run --on`
    /// grammar. Empty: the default set above.
    pub nodes: String,
    /// Users who may use it in a job. With `roles` empty too: anyone who
    /// may run the job.
    pub users: Vec<UserId>,
    pub roles: Vec<crate::Role>,
    pub by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl SecretScope {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-secret-scope-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Does this scope restrict who may use the secret?
    pub fn limits_users(&self) -> bool {
        !self.users.is_empty() || !self.roles.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecretRemoval {
    pub name: String,
    pub by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl SecretRemoval {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(b"mw-secret-remove-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}
