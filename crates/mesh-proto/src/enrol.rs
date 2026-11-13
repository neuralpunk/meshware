//! Enrolment: how a person's key becomes a mesh user (M23).
//!
//! meshware has no passwords and no login. **A person *is* an Ed25519
//! key**, and every request they make is signed by it. What was missing was
//! the conveyance: the key had to reach an admin somehow, and the product
//! offered no route, so it went over Slack as a 52-character string that
//! nobody could read aloud.
//!
//! Enrolment is that conveyance, and it is deliberately the *same shape* as
//! node pairing ([`crate::PairInit`], `mesh_core::pairing`): something that
//! holds a keypair asks to be recognised, both humans see the same short
//! code, and a human with authority looks at it and says yes.
//!
//! ## What a request is, and what it is not
//!
//! An [`EnrolmentRequest`] is **self-signed by the requesting key**. That
//! proves possession of the private half and is the only thing it proves.
//! It grants nothing, it is not a credential, and merging one changes no
//! authorization anywhere — it is a row in a queue. Approval is the
//! existing admin-signed `UserAdd` + `GrantAdd` pair, unchanged.
//!
//! That is what makes replicating requests safe: a node with no code for
//! the op ignores it and simply shows no pending queue, which is
//! indistinguishable from nobody having asked.
//!
//! ## Expiry is not optional
//!
//! A request nobody acted on disappears ([`DEFAULT_TTL_SECS`]). A
//! permanently growing queue of stale requests is how an approval queue
//! stops being read, and an approval queue that is not read is worse than
//! no queue at all.

use serde::{Deserialize, Serialize};

use crate::{NodeId, UserId};

/// How long a request stays in the queue before it is ignored everywhere.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Ceiling on a request's declared lifetime. A requester picks its own
/// expiry (its clock, its choice), so the merge path clamps it — otherwise
/// one request could pin itself in every node's queue forever.
pub const MAX_TTL_SECS: u64 = 24 * 3600;

/// Longest `suggested_username` a request may carry. It is a suggestion an
/// admin sees and can overwrite; it is not the username.
pub const MAX_SUGGESTED_USERNAME: usize = 64;

/// Longest reason a decline may carry.
pub const MAX_DECLINE_REASON: usize = 200;

/// Adjectives for the human-comparable code. 64 entries — 6 bits — chosen
/// to be short, unambiguous when read aloud, and free of homophones.
const ADJECTIVES: [&str; 64] = [
    "AMBER", "ANCIENT", "BOLD", "BRAVE", "BRIGHT", "BRISK", "CALM", "CLEAR", "CLEVER", "COSMIC",
    "CRISP", "DARING", "DEEP", "DIZZY", "EAGER", "EARLY", "EASY", "FAIR", "FANCY", "FIERCE",
    "FIRM", "FLAT", "FRESH", "GENTLE", "GLAD", "GOLDEN", "GRAND", "GREEN", "HAPPY", "HARDY",
    "HOLLOW", "HONEST", "HUMBLE", "IDLE", "IRON", "JOLLY", "KEEN", "KIND", "LATE", "LEVEL",
    "LIVELY", "LOYAL", "LUCKY", "MELLOW", "MERRY", "MIGHTY", "NEAT", "NIMBLE", "NOBLE", "PLAIN",
    "PROUD", "QUICK", "QUIET", "RAPID", "ROYAL", "RUGGED", "SHARP", "SILENT", "SILVER", "SOLID",
    "SPRY", "STEADY", "SUNNY", "TIDY",
];

/// Nouns for the human-comparable code. 64 entries, same rules: concrete,
/// one or two syllables, and no pair that sounds alike over a phone.
const NOUNS: [&str; 64] = [
    "ANCHOR", "BADGER", "BEACON", "BISON", "CANYON", "CEDAR", "COMET", "CORAL", "CRANE", "DELTA",
    "EAGLE", "EMBER", "FALCON", "FERRET", "FOREST", "GARNET", "GLACIER", "GRANITE", "HARBOR",
    "HERON", "IVORY", "JAGUAR", "KESTREL", "LANTERN", "LEDGER", "LEOPARD", "LILY", "MAGNET",
    "MAPLE", "MARBLE", "MEADOW", "MERIDIAN", "MONSOON", "OSPREY", "OTTER", "PELICAN", "PEPPER",
    "PILOT", "PLATEAU", "PRAIRIE", "PUFFIN", "QUARRY", "RAVEN", "RIDGE", "RIVER", "SALMON",
    "SAPLING", "SEQUOIA", "SHUTTLE", "SPARROW", "SUMMIT", "TALON", "TEMPEST", "THISTLE", "THUNDER",
    "TIMBER", "TUNDRA", "VALLEY", "VESSEL", "WALNUT", "WILLOW", "WOMBAT", "YARROW", "ZEPHYR",
];

/// The human-comparable enrolment code: `BRAVE-OTTER-4417`.
///
/// Derived from the requesting key and the request's nonce, so **both
/// sides compute it independently and neither transmits it** — the same
/// property that makes the pairing SAS worth reading aloud. It is a
/// *display encoding of the public key*, not a secret and not a token:
/// approving is still approving a specific `mwu1…`, and the code only
/// exists so that "read me the code on your screen" is a sentence a person
/// can actually say.
///
/// ~25 bits of comparison (64 × 64 × 10 000), which is more than the
/// 6-digit pairing code carries and is read aloud correctly far more
/// often — which is the entire point, because a code people mis-read is a
/// confirmation step people learn to skip.
pub fn enrolment_code(user: &UserId, nonce: &[u8; 16]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mw-enrol-code-v1");
    hasher.update(&user.0);
    hasher.update(nonce);
    let digest = hasher.finalize();
    let b = digest.as_bytes();
    let adjective = ADJECTIVES[(b[0] & 0x3f) as usize];
    let noun = NOUNS[(b[1] & 0x3f) as usize];
    let digits = u16::from_le_bytes([b[2], b[3]]) % 10_000;
    format!("{adjective}-{noun}-{digits:04}")
}

/// A keypair asking to be recognised by this mesh (M23 §3.2).
///
/// Self-signed: `signature` is by `user_id` over [`Self::signing_bytes`].
/// The merge path verifies that and nothing else about authority, because
/// there is no authority here to verify — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrolmentRequest {
    /// The person's public key. This *is* their identity; everything else
    /// in this record is context for the human who will approve it.
    pub user_id: UserId,
    /// What they would like to be called. A suggestion — the approving
    /// admin types the username that is actually bound.
    pub suggested_username: String,
    /// The node they asked from, which is the node that authors the op.
    /// The merge path requires the two to agree, so a node cannot claim a
    /// request came from somewhere else.
    pub from_node: NodeId,
    /// Randomness that makes the code unique per request, so a re-request
    /// after a decline does not show the admin a code they have already
    /// seen and dismissed.
    pub nonce: [u8; 16],
    pub created_at_micros: u64,
    /// When this request stops being shown. Clamped to
    /// [`MAX_TTL_SECS`] past `created_at_micros` on read.
    pub expires_at_micros: u64,
    /// Ed25519 signature by `user_id` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl EnrolmentRequest {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(b"mw-enrol-request-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// The code an admin and the requester compare.
    pub fn code(&self) -> String {
        enrolment_code(&self.user_id, &self.nonce)
    }

    /// When this request actually expires, with the requester's declared
    /// expiry clamped to a sane window around its own creation time.
    pub fn expires_at(&self) -> u64 {
        let ceiling = self
            .created_at_micros
            .saturating_add(MAX_TTL_SECS * 1_000_000);
        self.expires_at_micros
            .clamp(self.created_at_micros, ceiling)
    }

    pub fn is_expired_at(&self, now_micros: u64) -> bool {
        now_micros > self.expires_at()
    }
}

/// An admin declining a request (M23 §3.2).
///
/// A tombstone rather than a deletion, for the usual reason: a request that
/// vanished from one node's queue and not another's is a request two admins
/// disagree about. It suppresses every request from that key created at or
/// before `at_micros`, so declining does not bar the person from ever
/// asking again — only from re-appearing on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrolmentDecline {
    pub user_id: UserId,
    /// Free text shown to nobody but the audit log and the next admin to
    /// look. Optional in practice; empty is normal.
    pub reason: String,
    pub declined_by: UserId,
    pub at_micros: u64,
    /// Ed25519 signature by `declined_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl EnrolmentDecline {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-enrol-decline-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// One row of the pending queue (`meshnode users pending`, Director's
/// Access screen).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrolmentRow {
    pub user_id: UserId,
    pub suggested_username: String,
    /// The requesting node's name where it is known, its short id where it
    /// is not — a queue that says `mw1a4f…` for a machine everybody calls
    /// `laptop-mia` is a queue nobody can act on.
    pub from_node: String,
    pub code: String,
    pub requested_at_micros: u64,
    pub expires_at_micros: u64,
    /// Set when an admin has declined this key; the row is kept for as
    /// long as the request would have lived so that "I approved it, why is
    /// nothing happening" has an answer.
    pub declined: bool,
    pub declined_by: Option<String>,
    /// What an identity provider proved about this key, when the person
    /// ran `meshnode login` first (E1-P5).
    ///
    /// **`None` is the ordinary case and always will be**: SSO is optional,
    /// a mesh with no provider configured works exactly as it did, and the
    /// out-of-band code comparison is still the trust decision. This is
    /// evidence that makes an admin's decision better informed, not a
    /// second way to be authorized.
    pub verified: Option<VerifiedIdentity>,
}

/// The part of an [`crate::OidcAssertion`] an approving admin reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifiedIdentity {
    /// Which configured provider vouched.
    pub provider: String,
    /// The provider's stable identifier for the human. The thing to write
    /// down; emails get reassigned and `sub` does not.
    pub subject: String,
    pub email: String,
    pub display_name: String,
    pub groups: Vec<String>,
    /// What the provider's group mapping suggests. A default for the admin
    /// to accept or override, never applied on its own.
    pub suggested_role: Option<String>,
    /// The node that did the verification.
    pub verified_by: String,
    /// False once the assertion ages out: the evidence is stale and the
    /// person should log in again rather than the admin trusting it.
    pub fresh: bool,
}

/// What the requesting side is waiting on (`meshnode enrol`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EnrolmentState {
    /// No request from this key is in the queue, and it is not a user.
    None,
    /// In the queue, waiting on a human.
    Pending,
    /// An admin said no.
    Declined,
    /// The request outlived its expiry without a decision.
    Expired,
    /// Approved: the key is a mesh user now.
    Enrolled,
}

impl EnrolmentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EnrolmentState::None => "none",
            EnrolmentState::Pending => "pending",
            EnrolmentState::Declined => "declined",
            EnrolmentState::Expired => "expired",
            EnrolmentState::Enrolled => "enrolled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(b: u8) -> UserId {
        UserId([b; 32])
    }

    #[test]
    fn code_is_stable_and_shaped_for_humans() {
        let code = enrolment_code(&uid(7), &[3u8; 16]);
        assert_eq!(code, enrolment_code(&uid(7), &[3u8; 16]));
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 3, "{code}");
        assert_eq!(parts[2].len(), 4, "{code}");
        assert!(parts[2].chars().all(|c| c.is_ascii_digit()), "{code}");
        assert!(
            parts[0].chars().all(|c| c.is_ascii_uppercase()),
            "read aloud, so upper case and letters only: {code}"
        );
    }

    #[test]
    fn code_binds_to_both_key_and_nonce() {
        assert_ne!(
            enrolment_code(&uid(7), &[3u8; 16]),
            enrolment_code(&uid(8), &[3u8; 16]),
            "a different key must not show the same code"
        );
        assert_ne!(
            enrolment_code(&uid(7), &[3u8; 16]),
            enrolment_code(&uid(7), &[4u8; 16]),
            "a re-request must not reuse a code an admin already dismissed"
        );
    }

    #[test]
    fn wordlists_are_the_size_the_bit_extraction_assumes() {
        // 6 bits each: every masked index must land inside the list, and a
        // shorter list would silently make some codes unreachable.
        assert_eq!(ADJECTIVES.len(), 64);
        assert_eq!(NOUNS.len(), 64);
        let mut sorted = ADJECTIVES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 64, "duplicate adjective");
        let mut sorted = NOUNS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 64, "duplicate noun");
    }

    #[test]
    fn declared_expiry_is_clamped_to_the_window() {
        let mut req = EnrolmentRequest {
            user_id: uid(1),
            suggested_username: "mia".into(),
            from_node: NodeId([2u8; 32]),
            nonce: [0u8; 16],
            created_at_micros: 1_000_000,
            expires_at_micros: u64::MAX,
            signature: Vec::new(),
        };
        assert_eq!(req.expires_at(), 1_000_000 + MAX_TTL_SECS * 1_000_000);
        req.expires_at_micros = 0;
        assert_eq!(
            req.expires_at(),
            1_000_000,
            "an expiry before creation means expired, not eternal"
        );
        assert!(req.is_expired_at(2_000_000));
    }

    #[test]
    fn signing_bytes_ignore_the_signature_field() {
        let mut req = EnrolmentRequest {
            user_id: uid(1),
            suggested_username: "mia".into(),
            from_node: NodeId([2u8; 32]),
            nonce: [0u8; 16],
            created_at_micros: 1,
            expires_at_micros: 2,
            signature: Vec::new(),
        };
        let before = req.signing_bytes();
        req.signature = vec![9u8; 64];
        assert_eq!(before, req.signing_bytes());
    }
}
