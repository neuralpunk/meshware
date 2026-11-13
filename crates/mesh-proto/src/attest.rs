//! Proving a key was made on hardware and cannot leave it (Y2/Y3).
//!
//! ## Why this is the half that is worth money
//!
//! `mesh_core::hardware` can sign with a key an agent holds. What it cannot
//! do — what nothing that talks to an agent can do — is tell whether that
//! key is on a YubiKey or in `~/.ssh/id_ed25519`. The agent returns the
//! same 64 bytes either way, and a client that claimed "hardware" on the
//! strength of having found an agent would be asserting something it has no
//! way to know.
//!
//! An attestation certificate is the difference between
//!
//! > *we support YubiKeys*
//!
//! and
//!
//! > *every admin key in this mesh was generated on genuine hardware,
//! > cannot be exported, and here is the chain proving it.*
//!
//! The first is a checkbox on a questionnaire. The second is an audit
//! finding that closes, and it is the reason this is a replicated,
//! merge-verified record rather than a column some client fills in.
//!
//! ## How the chain is trusted
//!
//! **The root is replicated, not compiled in**, exactly like
//! [`crate::ops::OpBody::ReleaseKeyPut`] and for the same reasons. A mesh
//! decides which manufacturer it trusts by an admin act that lands in the
//! op log — auditable, revocable, and visible — rather than by whichever
//! certificates happened to be baked into the binary somebody installed.
//! That also means a mesh can trust its own CA, which is what an
//! organisation minting its own PIV cards actually needs, and it means this
//! feature ships without a hardcoded vendor certificate that nobody in the
//! tree could check by eye.
//!
//! A mesh that has trusted no roots verifies no attestations and shows no
//! `HARDWARE` column. That is the honest empty state: an unverified chain
//! is worth less than no chain, because it looks like proof.
//!
//! ## What is deliberately not claimed
//!
//! **An attestation is about the key, not about the moment.** It says this
//! public key was generated inside a device of this model with this touch
//! and PIN policy. It does not say the device is plugged in now, that a
//! human is present, or that this particular signature came from it —
//! [`crate::presence`] is the feature that answers those, and the two are
//! deliberately separate records answering separate questions.

use serde::{Deserialize, Serialize};

use crate::UserId;
use crate::rbac::tagged_bytes;

/// Which applet a key was attested by.
///
/// The two that produce a chain meshware can check. Both ultimately say the
/// same thing — *this key was generated in here and cannot come out* — and
/// they are distinguished because the slot vocabulary and the certificate
/// extensions differ, and an operator reading a list wants to know which
/// kind of device they are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestKind {
    /// PIV, typically slot 9c. YubiKey firmware 5.7 and later for Ed25519.
    Piv,
    /// The OpenPGP applet's key-generation attestation, or a card
    /// certificate. Ed25519 since firmware 5.2.3, so this is the wider
    /// fleet.
    OpenPgp,
    /// A FIDO2 authenticator's `makeCredential` attestation statement,
    /// flattened to its certificate chain.
    Fido2,
}

impl AttestKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttestKind::Piv => "piv",
            AttestKind::OpenPgp => "openpgp",
            AttestKind::Fido2 => "fido2",
        }
    }

    pub fn parse(s: &str) -> Option<AttestKind> {
        match s.to_ascii_lowercase().as_str() {
            "piv" => Some(AttestKind::Piv),
            "openpgp" | "gpg" | "pgp" => Some(AttestKind::OpenPgp),
            "fido2" | "fido" | "webauthn" => Some(AttestKind::Fido2),
            _ => None,
        }
    }
}

/// A certificate authority this mesh will accept attestations from.
///
/// Admin-signed and replicated, so trusting a manufacturer is one act with
/// a name on it rather than a file on every machine — and so a node that
/// joins next month arrives already knowing. The shape of
/// [`crate::ops::ReleaseKey`], because it is the same kind of decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttestRoot {
    /// What to call it. "Yubico PIV Attestation CA", "ACME issuing CA".
    pub label: String,
    /// The root or intermediate certificate, DER.
    pub certificate: Vec<u8>,
    pub added_by: UserId,
    pub added_at_micros: u64,
    pub signature: Vec<u8>,
}

impl AttestRoot {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-attest-root-v1", &unsigned)
    }

    /// The id this root is filed under: a digest of the certificate itself.
    ///
    /// Not the label, which is a human's word for it and can be typed twice
    /// differently. Two admins who add the same certificate have added the
    /// same root, and last writer wins over one record rather than leaving
    /// two that must both be revoked.
    pub fn id(&self) -> String {
        crate::hex32(blake3::hash(&self.certificate).as_bytes())
    }
}

/// One person's claim that their key lives on hardware, with the chain.
///
/// **Self-signed, and that is the control** — the same argument
/// [`crate::presence::TokenCredential`] makes. Attesting a key is a
/// statement about your own identity, so it carries your signature and
/// nobody else's would mean anything. An admin cannot attest a key into
/// somebody else's account, because then "hardware" would be satisfiable by
/// whoever holds the admin key, which is the person the whole column exists
/// to constrain.
///
/// The signature is over the chain as well as the claim, so the record and
/// the evidence stand or fall together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareAttestation {
    /// Whose key. Must be the key the leaf certificate is *for*, checked at
    /// merge — an attestation of somebody else's key proves nothing here.
    pub user_id: UserId,
    pub kind: AttestKind,
    /// Leaf first, then any intermediates, DER. The root is not included:
    /// the mesh supplies that from its own trusted set, because a chain
    /// that carries its own root proves nothing at all.
    pub chain: Vec<Vec<u8>>,
    /// Where on the device, as the device names it. `9c`, `auth`, `sig`.
    /// Recorded verbatim for a person to read; never parsed.
    pub slot: String,
    pub added_at_micros: u64,
    pub signature: Vec<u8>,
}

impl HardwareAttestation {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-attest-v1", &unsigned)
    }
}

/// Withdrawing one: a key that was rotated out, or a root that turned out
/// not to mean what somebody thought.
///
/// Signed by the owner **or** an admin. Unlike the attestation itself, this
/// direction has to be available to somebody else: an attestation nobody
/// can withdraw because its owner left is a column that lies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttestRevocation {
    pub user_id: UserId,
    pub reason: String,
    pub revoked_by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl AttestRevocation {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-attest-revoke-v1", &unsigned)
    }
}

/// The bytes an [`crate::ops::OpBody::AttestRootRemove`] signature covers.
///
/// A free function rather than a struct method, matching
/// `policy_remove_signing_bytes`: the op carries loose fields because there
/// is no record to remove — only an id, a signer and a clock.
pub fn attest_root_remove_signing_bytes(id: &str, by: &UserId, at_micros: u64) -> Vec<u8> {
    tagged_bytes(b"mw-attest-root-remove-v1", &(id, by, at_micros))
}

/// How big a chain may be, in certificates and in bytes.
///
/// An op goes into the replicated log on every node forever, so a chain is
/// bounded before it is verified rather than after. Four is generous: a
/// YubiKey PIV attestation is leaf plus one intermediate.
pub const MAX_CHAIN_CERTS: usize = 4;
pub const MAX_CERT_BYTES: usize = 8 * 1024;

impl HardwareAttestation {
    /// Is this the right shape to bother verifying?
    ///
    /// Checked at merge before any parsing, so a malformed record never
    /// reaches the X.509 code and every node refuses it identically.
    pub fn well_formed(&self) -> bool {
        !self.chain.is_empty()
            && self.chain.len() <= MAX_CHAIN_CERTS
            && self
                .chain
                .iter()
                .all(|c| !c.is_empty() && c.len() <= MAX_CERT_BYTES)
            && self.slot.len() <= 32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_is_filed_under_its_certificate_not_its_label() {
        let a = AttestRoot {
            label: "Yubico PIV Attestation CA".into(),
            certificate: vec![1, 2, 3],
            added_by: UserId([1u8; 32]),
            added_at_micros: 1,
            signature: Vec::new(),
        };
        let same_cert_other_label = AttestRoot {
            label: "yubico piv ca".into(),
            added_by: UserId([2u8; 32]),
            added_at_micros: 99,
            ..a.clone()
        };
        assert_eq!(
            a.id(),
            same_cert_other_label.id(),
            "two admins adding the same certificate have added one root"
        );
        let other = AttestRoot {
            certificate: vec![4, 5, 6],
            ..a.clone()
        };
        assert_ne!(a.id(), other.id());
    }

    #[test]
    fn a_chain_is_bounded_before_it_is_parsed() {
        let base = HardwareAttestation {
            user_id: UserId([1u8; 32]),
            kind: AttestKind::Piv,
            chain: vec![vec![0u8; 64]],
            slot: "9c".into(),
            added_at_micros: 1,
            signature: Vec::new(),
        };
        assert!(base.well_formed());
        assert!(
            !HardwareAttestation {
                chain: Vec::new(),
                ..base.clone()
            }
            .well_formed(),
            "an attestation with no chain attests nothing"
        );
        assert!(
            !HardwareAttestation {
                chain: vec![vec![0u8; 64]; MAX_CHAIN_CERTS + 1],
                ..base.clone()
            }
            .well_formed()
        );
        assert!(
            !HardwareAttestation {
                chain: vec![vec![0u8; MAX_CERT_BYTES + 1]],
                ..base.clone()
            }
            .well_formed()
        );
    }

    #[test]
    fn the_kinds_round_trip_through_their_names() {
        for k in [AttestKind::Piv, AttestKind::OpenPgp, AttestKind::Fido2] {
            assert_eq!(AttestKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(AttestKind::parse("tpm"), None);
    }
}
