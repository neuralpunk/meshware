//! The mesh genesis document: the trust anchor every node stores
//! (MASTERPLAN §4.2). Created automatically on first `meshnode up` —
//! zero-config security means bootstrap asks the user nothing (§5.1).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::identity::{Keypair, verify_signature};
use mesh_proto::{MeshId, NodeId};

const GENESIS_FILE: &str = "genesis.json";

/// The signed body of the genesis document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisBody {
    pub mesh_id: MeshId,
    pub mesh_name: String,
    pub created_at_micros: u64,
    /// Founding admin's *user* public key. User identity is separate from
    /// node identity so RBAC (M3) attaches to people, not machines.
    pub founder_user: [u8; 32],
    /// The node that bootstrapped the mesh: the authorization anchor for
    /// the very first membership op (MASTERPLAN §4.4).
    pub founder_node: NodeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genesis {
    pub body: GenesisBody,
    /// Ed25519 signature (64 bytes) by `founder_user` over the postcard
    /// encoding of `body`. Stored as a Vec because serde's derive does not
    /// implement arrays past 32 elements.
    pub signature: Vec<u8>,
}

impl Genesis {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("mesh").join(GENESIS_FILE)
    }

    pub fn create(mesh_name: &str, founder: &Keypair, founder_node: NodeId) -> Result<Self> {
        let mut mesh_id = [0u8; 16];
        getrandom::fill(&mut mesh_id)
            .map_err(|e| CoreError::Genesis(format!("system rng unavailable: {e}")))?;
        let body = GenesisBody {
            mesh_id: MeshId(mesh_id),
            mesh_name: mesh_name.to_string(),
            created_at_micros: mesh_proto::now_micros(),
            founder_user: founder.public_bytes(),
            founder_node,
        };
        let encoded = postcard_encode(&body)?;
        let signature = founder.sign(&encoded).to_vec();
        Ok(Genesis { body, signature })
    }

    pub fn verify(&self) -> Result<()> {
        let encoded = postcard_encode(&self.body)?;
        let sig: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| CoreError::Genesis("genesis signature has wrong length".into()))?;
        if verify_signature(&self.body.founder_user, &encoded, &sig) {
            Ok(())
        } else {
            Err(CoreError::Genesis("genesis signature invalid".into()))
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| CoreError::Genesis(format!("encode: {e}")))?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(data_dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        let genesis: Genesis = serde_json::from_slice(&bytes)
            .map_err(|e| CoreError::Genesis(format!("{}: {e}", path.display())))?;
        genesis.verify()?;
        Ok(Some(genesis))
    }

    /// Load the existing genesis or bootstrap a new mesh. Returns
    /// `(genesis, bootstrapped_now)`.
    ///
    /// `founder` is `None` when this node holds no openable user key — it
    /// did not found the mesh, or the key is sealed (B9). Loading an
    /// existing genesis does not need one; **founding a new mesh does**, and
    /// is refused rather than done with a key nobody can produce. Founding
    /// with a substitute would create a mesh whose founding admin is an
    /// identity no human holds, which is unrecoverable and would look like
    /// success.
    pub fn load_or_bootstrap(
        data_dir: &Path,
        mesh_name: &str,
        founder: Option<&Keypair>,
        founder_node: NodeId,
    ) -> Result<(Self, bool)> {
        if let Some(g) = Self::load(data_dir)? {
            return Ok((g, false));
        }
        let Some(founder) = founder else {
            return Err(CoreError::Genesis(
                "this node has no mesh yet and its user key is locked, so there is no \
                 identity to found one as.\n\n\
                 Unlock it first:\n    meshnode identity unlock\n\n\
                 Joining an existing mesh does not need the key unlocked — only founding does."
                    .into(),
            ));
        };
        let g = Self::create(mesh_name, founder, founder_node)?;
        g.save(data_dir)?;
        Ok((g, true))
    }
}

fn postcard_encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_stdvec(value).map_err(|e| CoreError::Genesis(format!("encode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_then_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let founder = Keypair::generate().expect("keypair");
        let node = Keypair::generate().expect("node keypair");
        let (g1, created) =
            Genesis::load_or_bootstrap(dir.path(), "hyacinth", Some(&founder), node.node_id())
                .expect("bootstrap");
        assert!(created);
        assert_eq!(g1.body.mesh_name, "hyacinth");
        let (g2, created2) =
            Genesis::load_or_bootstrap(dir.path(), "ignored", Some(&founder), node.node_id())
                .expect("reload");
        assert!(!created2);
        assert_eq!(g1.body.mesh_id, g2.body.mesh_id);
        assert_eq!(g2.body.mesh_name, "hyacinth");
    }

    /// Founding needs a founder; joining does not.
    ///
    /// Bootstrapping with a substitute key would create a mesh whose
    /// founding admin is an identity no human holds — unrecoverable, and it
    /// would look like success. Loading an existing genesis needs no key at
    /// all, which is what lets a node with a sealed key still boot.
    #[test]
    fn a_locked_key_cannot_found_a_mesh_but_does_not_stop_loading_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = Keypair::generate().expect("node keypair");

        let err = Genesis::load_or_bootstrap(dir.path(), "nope", None, node.node_id())
            .expect_err("founding without a founder key must be refused");
        assert!(
            err.to_string().contains("identity unlock"),
            "the refusal should say how to fix it: {err}"
        );
        assert!(
            Genesis::load(dir.path()).expect("load").is_none(),
            "a refused bootstrap must not leave a genesis behind"
        );

        // Found it properly, then prove a later boot needs no key.
        let founder = Keypair::generate().expect("keypair");
        let (g1, created) =
            Genesis::load_or_bootstrap(dir.path(), "hyacinth", Some(&founder), node.node_id())
                .expect("bootstrap");
        assert!(created);
        let (g2, created2) =
            Genesis::load_or_bootstrap(dir.path(), "ignored", None, node.node_id())
                .expect("an existing mesh loads without a founder key");
        assert!(!created2);
        assert_eq!(g1.body.mesh_id, g2.body.mesh_id);
    }

    #[test]
    fn tampered_genesis_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let founder = Keypair::generate().expect("keypair");
        let node = Keypair::generate().expect("node keypair");
        let (mut g, _) =
            Genesis::load_or_bootstrap(dir.path(), "hyacinth", Some(&founder), node.node_id())
                .expect("bootstrap");
        g.body.mesh_name = "evil".into();
        g.save(dir.path()).expect("save tampered");
        assert!(Genesis::load(dir.path()).is_err());
    }
}
