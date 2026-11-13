//! Fleet upgrade (M24 §4).
//!
//! The binary and the definitions are two things that must never become one
//! thing. Definitions replicate through the op-log automatically; binaries
//! never do, and this module is the whole of what a fleet upgrade *is*
//! allowed to be.
//!
//! ## A plan is an authorization, never a trigger
//!
//! An [`UpgradePlan`] says: *these nodes may be asked to become this
//! version, until then, on this admin's signature.* It replicates like any
//! other definition, and **nothing acts on it by itself**. A node that was
//! offline through a rollout and comes back to find a plan in its state
//! does exactly nothing. That inertness is the difference between
//! orchestration and auto-update, and auto-update is the thing M24 §5 says
//! not to build: a mesh that upgrades itself is a mesh that can break
//! itself at 3am with nobody in the loop.
//!
//! ## Why it is a record and not a live envelope
//!
//! The obvious design — the operator signs one IPC envelope and the driver
//! forwards it to each node — fails twice on a real rollout. An envelope's
//! freshness window is five minutes and a serial rollout across a fleet is
//! not; and the replay guard that stops a stolen envelope from being
//! re-submitted also stops the *driver* from polling the same node twice.
//!
//! So authority is re-derived on the acting node from its own replicated
//! state, exactly as M8 does for a schedule's fire, and with the same
//! benefit: revoking the admin stops the rollout on every node it has not
//! reached, and there is no signature anywhere that has to be recalled.

use serde::{Deserialize, Serialize};

use crate::{BundleId, UpgradeId, UserId};

/// A meshnode release living **in the mesh** rather than on a server
/// (M24 §9).
///
/// ## Why this is not the push channel §5 refuses
///
/// §5 refuses "a path where one node hands another node an executable",
/// and it is right to: a node that runs what a peer gives it is a node one
/// compromise away from every other. This is the opposite shape.
///
/// The bytes travel as an ordinary **content-addressed bundle** — pulled
/// by digest, every chunk verified against its hash on write, from
/// whichever holder answers first. The serving peer is a CDN, not an
/// authority, and it is trusted with exactly nothing: the receiving node
/// verifies the **minisign signature by the release key** against a key it
/// holds locally before the binary is allowed anywhere near `rename(2)`.
///
/// That is the same trust model the HTTP channel always had. A release
/// server was never trusted either — it is why minisign exists — so moving
/// the bytes onto the mesh adds no new trust and removes a single point of
/// failure. It is also the model the mesh already uses for every service
/// it runs: a bundle *is* executable content delivered node to node, and
/// what makes it safe is a signature, not the transport.
///
/// **Two independent keys.** Publishing is admin-signed *and* the binary
/// must carry a release-key signature, so a compromised mesh admin cannot
/// introduce a binary and a stolen release key cannot introduce one
/// without an admin. Neither key alone is enough.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseRecord {
    /// Bare version, as a daemon reports it (`0.6.0`, never `v0.6.0`).
    pub version: String,
    /// The bundle holding the binary. Replicates lazily like any other
    /// content: known to everyone immediately, fetched when needed — or
    /// pre-staged fleet-wide with `meshnode bundles prefetch`.
    pub bundle_id: BundleId,
    /// Path of the binary inside the bundle.
    pub artifact: String,
    /// The `.minisig` document, verbatim. Carried in the record rather
    /// than in the bundle so a node can check *what it is about to fetch*
    /// is signed before spending a hundred megabytes finding out.
    pub minisig: String,
    /// BLAKE3 of the binary, so the record pins one exact file and a
    /// node can refuse a bundle whose contents changed shape.
    pub digest: [u8; 32],
    pub published_by: UserId,
    pub published_at_micros: u64,
    /// Ed25519 signature by `published_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl ReleaseRecord {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-release-publish-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// A minisign public key this mesh trusts to sign meshnode releases
/// (M24 §9).
///
/// ## Why trust replicates
///
/// The key a node verifies a release against used to live only in a file
/// on that node — `[release] pubkey`, or the key compiled into the binary.
/// That is a per-machine chore exactly when there are many machines, and
/// worse, it is a chore whose omission is *invisible*: the node looks
/// healthy, joins, gossips, runs jobs, and refuses its first upgrade
/// months later with an error about a placeholder key. A node that joined
/// last week has no way to have been told.
///
/// Trust in this system is admin-signed replicated state — that is what a
/// user is, what a grant is, what a plan is. A release key is trust, so it
/// is one of those: added once by an admin, carried to every node
/// including the ones that join afterwards, revocable the same way and
/// with the same effect everywhere it has reached.
///
/// ## It does not weaken the two-key rule
///
/// [`ReleaseRecord`] requires two independent signatures: an admin's, over
/// the record, and the release key's, over the binary. This op is
/// admin-signed and only says *which release keys count*. An admin who
/// adds their own key could already publish releases with it — the config
/// file was never the thing standing between an admin and a binary. What
/// this removes is a file, not a check.
///
/// A node still refuses a release no trusted key signed, and the local
/// `[release] pubkey` still wins where it is set: a machine owner's
/// written-down answer outranks the mesh's.
/// A second signature over a release, made by a key on a token (Y6).
///
/// **The one secret whose loss ends the project.** `SECURITY.md` states
/// that a node verifies every binary against the minisign release key and
/// that the check cannot be turned off — so whoever holds that key can push
/// a binary to every fleet running meshware, and today it is a file CI can
/// read. minisign has no hardware support, so the key cannot be moved; what
/// can be done is to require a signature a file cannot produce.
///
/// Its own record rather than a field on [`ReleaseRecord`], for the usual
/// reason: that struct is signed and postcard encodes positionally, so an
/// appended field would invalidate every release signature ever made.
/// Keyed by the binary's digest, which is what both signatures are about —
/// so a co-signature can be published before, after, or by somebody other
/// than whoever published the release.
///
/// The `armored` document is `ssh-keygen -Y sign -n meshware-release` over
/// the binary itself. Verified by `mesh_core::sshsig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseCosignature {
    /// BLAKE3 of the binary — [`ReleaseRecord::digest`].
    pub digest: [u8; 32],
    /// The `-----BEGIN SSH SIGNATURE-----` document, verbatim.
    pub armored: String,
    /// Who published it. Not who *signed* it: the signing key is inside the
    /// document, and that is what the mesh checks.
    pub published_by: UserId,
    pub published_at_micros: u64,
    /// Ed25519 by `published_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl ReleaseCosignature {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-release-cosign-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// The key this record is filed under.
    pub fn key(&self) -> String {
        crate::hex32(&self.digest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseKey {
    /// The base64 minisign public key line (`RW…`) — the second line of a
    /// `.pub`, without the untrusted comment.
    pub pubkey: String,
    /// What this key is, for whoever reads the list a year from now.
    pub label: String,
    pub added_by: UserId,
    pub added_at_micros: u64,
    /// Ed25519 signature by `added_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl ReleaseKey {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(b"mw-release-key-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// A minisign key line is base64 over a 42-byte blob and carries no
    /// whitespace. Checked before it is signed and again at merge, so a
    /// pasted comment line or a truncated key is refused by the person who
    /// pasted it rather than by every node quietly failing to verify.
    pub fn looks_like_a_key(pubkey: &str) -> bool {
        ReleaseKey::looks_like_minisign(pubkey) || ReleaseKey::looks_like_ssh(pubkey)
    }

    /// The minisign form: the `RW…` line of a `.pub`.
    pub fn looks_like_minisign(pubkey: &str) -> bool {
        let k = pubkey.trim();
        k.len() >= 40
            && k.len() <= 120
            && k.starts_with("RW")
            && !k.to_ascii_lowercase().contains("placeholder")
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    }

    /// The SSH form: a co-signing key for Y6.
    ///
    /// **The same list holds both**, told apart by shape, which is what
    /// lets a whole second signature scheme arrive without a second record
    /// type, a second op, or a second admin verb. A minisign line starts
    /// `RW` and an SSH one names its algorithm first, so the two can never
    /// be confused for one another.
    ///
    /// Shape only — the base64 is checked where it is used, because a key
    /// that parses and a key that verifies are different questions and the
    /// second one belongs next to the signature.
    pub fn looks_like_ssh(pubkey: &str) -> bool {
        let k = pubkey.trim();
        let Some((alg, rest)) = k.split_once(' ') else {
            return false;
        };
        if alg != "ssh-ed25519" && alg != "sk-ssh-ed25519@openssh.com" {
            return false;
        }
        let blob = rest.split_whitespace().next().unwrap_or("");
        blob.len() >= 32
            && blob.len() <= 256
            && blob
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    }
}

/// The bytes a release-key revocation's signature covers.
pub fn release_key_revoke_signing_bytes(pubkey: &str, by: &UserId, at_micros: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(b"mw-release-key-revoke-v1");
    buf.extend_from_slice(pubkey.as_bytes());
    buf.extend_from_slice(&by.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf
}

/// Longest a plan stays usable. A standing authorization to replace a
/// binary is not something to leave lying around, and a rollout that has
/// not finished in a day has a person to talk to rather than a deadline to
/// extend.
pub const MAX_PLAN_TTL_SECS: u64 = 24 * 3600;

/// Default lifetime of a plan.
pub const DEFAULT_PLAN_TTL_SECS: u64 = 6 * 3600;

/// Ceiling on how many nodes a single wave may touch at once.
///
/// The point of a wave is that the fleet keeps working while it rolls, and
/// that stops being true past some fraction of it. This is a hard bound
/// rather than advice: a plan asking for a thousand-node wave is asking for
/// the thing `any` is refused for, spelled differently.
pub const MAX_BATCH: u32 = 64;

/// A standing, admin-signed authorization to roll one release across one
/// ring (M24 §4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradePlan {
    pub plan_id: UpgradeId,
    /// The ring. **`any` is refused** where plans are created: naming a
    /// whole mesh in one record is the thing this is built not to do.
    pub selector: String,
    /// The release, as a daemon reports it (`0.6.0`, never `v0.6.0`) — so
    /// a rollout cannot compare a tag against a version and conclude a node
    /// never upgraded.
    pub version: String,
    /// Pass `--move` to each node's drain: re-dispatch queued jobs rather
    /// than waiting them out. The operator's choice, carried here because
    /// the driver is not a party to it.
    pub move_pending: bool,
    /// How many nodes may be upgrading at once. `0` and `1` both mean one
    /// at a time; clamped to [`MAX_BATCH`] on read.
    ///
    /// Serial is the right default and the wrong *only* option: a
    /// thousand-node fleet at one node a minute is a rollout nobody
    /// finishes in a working day, and an operator who cannot express
    /// "thirty at a time" will express it by running thirty rollouts, which
    /// is the same concurrency with none of the health gating.
    ///
    /// The gate does not weaken with width — a wave must be **wholly**
    /// back, alive and caught up before the next one starts, so the blast
    /// radius of a bad binary is one wave and never the fleet.
    pub batch: u32,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// After this, no node will act on the plan. Clamped to
    /// [`MAX_PLAN_TTL_SECS`] past creation on read.
    pub expires_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl UpgradePlan {
    /// The bytes an ordinary plan's signature covers.
    pub fn signing_bytes(&self) -> Vec<u8> {
        self.signing_bytes_forced(false)
    }

    /// The bytes the signature covers, domain-separated on whether the
    /// plan is published as a **forced** one — one that re-installs even
    /// on a node already reporting its version (§4).
    ///
    /// Forcing is not a field of the record. It is which op body carried
    /// the record — [`crate::OpBody::UpgradePlanForce`] rather than
    /// `UpgradePlanPut` — which is what keeps a plan's encoding exactly as
    /// it has always been, so every older node goes on reading ordinary
    /// plans after this existed. The cost of putting it outside the record
    /// is that the signature would not cover it, and an admin's ordinary
    /// plan could be lifted off the gossip stream and republished as a
    /// forced one by anybody. This prefix is what pays that cost: the two
    /// kinds of plan sign different bytes, so neither verifies as the
    /// other.
    pub fn signing_bytes_forced(&self, force: bool) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(if force {
            b"mw-upgrade-plan-forced-v1".as_slice()
        } else {
            b"mw-upgrade-plan-v1".as_slice()
        });
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    pub fn expires_at(&self) -> u64 {
        let ceiling = self
            .created_at_micros
            .saturating_add(MAX_PLAN_TTL_SECS * 1_000_000);
        self.expires_at_micros
            .clamp(self.created_at_micros, ceiling)
    }

    pub fn is_expired_at(&self, now_micros: u64) -> bool {
        now_micros > self.expires_at()
    }

    /// Nodes per wave, clamped. Read on the *driving* node, so a plan
    /// authored by a newer build asking for a wider wave than this one
    /// allows is narrowed rather than obeyed.
    pub fn wave(&self) -> usize {
        self.batch.clamp(1, MAX_BATCH) as usize
    }
}

/// The bytes a cancellation's signature covers.
pub fn upgrade_cancel_signing_bytes(plan_id: &UpgradeId, by: &UserId, at_micros: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-upgrade-cancel-v1");
    buf.extend_from_slice(&plan_id.0);
    buf.extend_from_slice(&by.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> UpgradePlan {
        UpgradePlan {
            plan_id: UpgradeId([1u8; 8]),
            selector: "tag=canary".into(),
            version: "0.6.0".into(),
            move_pending: false,
            batch: 1,
            created_by: UserId([2u8; 32]),
            created_at_micros: 1_000_000,
            expires_at_micros: u64::MAX,
            signature: Vec::new(),
        }
    }

    #[test]
    fn a_standing_authorization_does_not_stand_forever() {
        let p = plan();
        assert_eq!(p.expires_at(), 1_000_000 + MAX_PLAN_TTL_SECS * 1_000_000);
        assert!(p.is_expired_at(1_000_000 + (MAX_PLAN_TTL_SECS + 1) * 1_000_000));
    }

    #[test]
    fn a_wave_is_at_least_one_and_never_the_fleet() {
        let mut p = plan();
        p.batch = 0;
        assert_eq!(p.wave(), 1, "0 means serial, never zero-width");
        p.batch = 1;
        assert_eq!(p.wave(), 1);
        p.batch = 32;
        assert_eq!(p.wave(), 32);
        p.batch = 100_000;
        assert_eq!(
            p.wave(),
            MAX_BATCH as usize,
            "a thousand-node wave is `any` spelled differently"
        );
    }

    #[test]
    fn signing_bytes_ignore_the_signature_field() {
        let mut p = plan();
        let before = p.signing_bytes();
        p.signature = vec![9u8; 64];
        assert_eq!(before, p.signing_bytes());
        // …and cover everything a node acts on.
        let mut other = plan();
        other.version = "0.7.0".into();
        assert_ne!(before, other.signing_bytes());
        let mut other = plan();
        other.selector = "any".into();
        assert_ne!(before, other.signing_bytes());
    }

    #[test]
    fn a_forced_plan_signs_different_bytes() {
        // Forcing rides the op body, not the record, so the record's
        // encoding is untouched — that is what lets an older node go on
        // reading ordinary plans. The signature has to cover it anyway,
        // or an ordinary plan could be republished as a forced one by
        // anybody who saw it gossip past.
        let p = plan();
        assert_eq!(p.signing_bytes(), p.signing_bytes_forced(false));
        assert_ne!(p.signing_bytes_forced(false), p.signing_bytes_forced(true));
    }
}
