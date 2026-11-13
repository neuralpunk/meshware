//! Typed identifiers. All serde impls are format-aware: human-readable
//! formats (JSON) carry display strings, binary formats (postcard) carry
//! compact raw bytes.

use serde::{Deserialize, Serialize};

use crate::ProtoError;

pub(crate) const ID_ENCODING: data_encoding::Encoding = data_encoding::BASE32_NOPAD;

macro_rules! human_readable_serde {
    ($ty:ident, $len:expr) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.serialize_str(&self.to_display_string())
                } else {
                    self.0.serialize(s)
                }
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    let s = String::deserialize(d)?;
                    $ty::parse(&s).map_err(serde::de::Error::custom)
                } else {
                    Ok($ty(<[u8; $len]>::deserialize(d)?))
                }
            }
        }
    };
}

fn decode_body<const N: usize>(s: &str, prefix: &str) -> Result<[u8; N], ProtoError> {
    let body = s
        .strip_prefix(prefix)
        .ok_or_else(|| ProtoError::InvalidId(s.to_string()))?;
    let bytes = ID_ENCODING
        .decode(body.to_uppercase().as_bytes())
        .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| ProtoError::InvalidId(s.to_string()))
}

fn encode_body(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", ID_ENCODING.encode(bytes).to_lowercase())
}

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// A node's identity: its Ed25519 public key. The key *is* the identity —
/// there is no CA (MASTERPLAN §4.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    pub fn to_display_string(&self) -> String {
        encode_body("mw1", &self.0)
    }

    /// Short fingerprint for logs and tables.
    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("mw1{}", &s[..12.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "mw1").map(NodeId)
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

impl std::str::FromStr for NodeId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(NodeId, 32);

// ---------------------------------------------------------------------------
// UserId
// ---------------------------------------------------------------------------

/// A user's identity: their Ed25519 public key, separate from node keys so
/// RBAC attaches to people, not machines (MASTERPLAN §7.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(pub [u8; 32]);

impl UserId {
    pub fn to_display_string(&self) -> String {
        encode_body("mwu1", &self.0)
    }

    /// Short fingerprint for logs and tables.
    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("mwu1{}", &s[..12.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "mwu1").map(UserId)
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UserId({})", self.short())
    }
}

impl std::str::FromStr for UserId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(UserId, 32);

// ---------------------------------------------------------------------------
// GrantId
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GrantId(pub [u8; 8]);

impl GrantId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        GrantId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("grant-", &self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "grant-").map(GrantId)
    }
}

impl std::fmt::Display for GrantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for GrantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GrantId({})", self.to_display_string())
    }
}

impl std::str::FromStr for GrantId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(GrantId, 8);

// ---------------------------------------------------------------------------
// UpgradeId (M24 §4)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UpgradeId(pub [u8; 8]);

impl UpgradeId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        UpgradeId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("upg-", &self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "upg-").map(UpgradeId)
    }
}

impl std::fmt::Display for UpgradeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for UpgradeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UpgradeId({})", self.to_display_string())
    }
}

impl std::str::FromStr for UpgradeId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(UpgradeId, 8);

// ---------------------------------------------------------------------------
// AuditId: 6-byte unix-millis timestamp + 10 random bytes, like JobId —
// sortable by time so the audit log reads chronologically.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuditId(pub [u8; 16]);

impl AuditId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        AuditId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("aud-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("aud-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "aud-").map(AuditId)
    }
}

impl std::fmt::Display for AuditId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for AuditId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuditId({})", self.short())
    }
}

impl std::str::FromStr for AuditId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(AuditId, 16);

// ---------------------------------------------------------------------------
// ApprovalId: identifies one N-of-M approval request (M11 §5.2). Same
// time-sortable layout as AuditId, deliberately: an approvals inbox is
// read newest-first exactly like an audit log, and a request that sorts
// by its id sorts by when it was asked for.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ApprovalId(pub [u8; 16]);

impl ApprovalId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        ApprovalId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("apr-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("apr-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "apr-").map(ApprovalId)
    }
}

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApprovalId({})", self.short())
    }
}

impl std::str::FromStr for ApprovalId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(ApprovalId, 16);

// ---------------------------------------------------------------------------
// FanId: identifies one fan-out submission — the logical job whose
// sub-results are per-target JobIds (MASTERPLAN §9.3). Same time-sortable
// layout as JobId/AuditId.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FanId(pub [u8; 16]);

impl FanId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        FanId(bytes)
    }

    /// The fan-out group id of one attempt at one fan-out step (M8 §3.3).
    ///
    /// The [`JobId::derive`] argument, applied to the other half of a
    /// planned step: a fan-out step pre-allocates a *group* rather than
    /// per-target jobs, and two independently built plans for one run must
    /// agree on it for the same reason.
    pub fn derive(run: &RunId, step: &str, attempt: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"mw-run-step-fan-v1");
        hasher.update(&run.0);
        hasher.update(&(step.len() as u64).to_le_bytes());
        hasher.update(step.as_bytes());
        hasher.update(&attempt.to_le_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&run.0[..6]);
        bytes[6..].copy_from_slice(&digest.as_bytes()[..10]);
        FanId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("fan-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("fan-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "fan-").map(FanId)
    }
}

impl std::fmt::Display for FanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for FanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FanId({})", self.short())
    }
}

impl std::str::FromStr for FanId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(FanId, 16);

// ---------------------------------------------------------------------------
// RunId: identifies one workflow run — the logical unit whose per-step
// results are ordinary JobIds (M10 §4.2). Same time-sortable layout as
// JobId/FanId, deliberately: a run *is* the workflow-shaped sibling of a
// fan-out group, and reusing the id shape is the smallest visible sign
// that it reuses the ownership model too.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub [u8; 16]);

impl RunId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        RunId(bytes)
    }

    /// The run id of one occurrence of a schedule (M8 §3.3).
    ///
    /// Deterministic, so two nodes that both believe they are the elected
    /// firer for the same occurrence name the *same run* rather than two.
    /// That is the whole of the milestone's exactly-once story: a double
    /// fire converges instead of duplicating, which is the only one of
    /// {prevent, detect, converge} actually available across a partition.
    ///
    /// The layout matches [`Self::generate`] — a big-endian millisecond
    /// prefix then 10 distinguishing bytes — so a scheduled run sorts into
    /// history at the time it was *meant* to run, not the time some node
    /// noticed it was due.
    pub fn derive(schedule: &str, occurrence_micros: u64) -> Self {
        let millis = occurrence_micros / 1000;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"mw-schedule-run-v1");
        hasher.update(&(schedule.len() as u64).to_le_bytes());
        hasher.update(schedule.as_bytes());
        hasher.update(&occurrence_micros.to_le_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        bytes[6..].copy_from_slice(&digest.as_bytes()[..10]);
        RunId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("run-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("run-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "run-").map(RunId)
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunId({})", self.short())
    }
}

impl std::str::FromStr for RunId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(RunId, 16);

// ---------------------------------------------------------------------------
// MeshId
// ---------------------------------------------------------------------------

/// Mesh identity: 16 random bytes minted at genesis.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MeshId(pub [u8; 16]);

impl MeshId {
    pub fn to_display_string(&self) -> String {
        encode_body("mesh-", &self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "mesh-").map(MeshId)
    }
}

impl std::fmt::Display for MeshId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for MeshId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MeshId({})", self.to_display_string())
    }
}

human_readable_serde!(MeshId, 16);

// ---------------------------------------------------------------------------
// JobId: 6-byte unix-millis timestamp + 10 random bytes. Sortable by
// creation time, collision-safe, no coordination (MASTERPLAN §15 "IDs").
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub [u8; 16]);

impl JobId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        // Best-effort randomness; the timestamp prefix already spreads keys.
        let _ = getrandom::fill(&mut bytes[6..]);
        JobId(bytes)
    }

    /// The job id of one attempt at one step of one run (M8 §3.3).
    ///
    /// Every workflow run uses this, not only scheduled ones. A plan has
    /// always pre-allocated its step job ids so a re-dispatch (adoption,
    /// restart, retry) recognises work it already started
    /// ([`crate::PlannedStep`]); deriving them additionally makes two
    /// *independently built* plans for the same run agree on every id. A
    /// step dispatched by a node that then loses the plan merge therefore
    /// carries the id the winner would have used, and the executor's
    /// journaled-acceptance rule collapses it to one execution.
    ///
    /// The millisecond prefix comes from the run, so a run's jobs stay
    /// clustered in the time-ordered key space the way `generate` intends.
    pub fn derive(run: &RunId, step: &str, attempt: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"mw-run-step-job-v1");
        hasher.update(&run.0);
        hasher.update(&(step.len() as u64).to_le_bytes());
        hasher.update(step.as_bytes());
        hasher.update(&attempt.to_le_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&run.0[..6]);
        bytes[6..].copy_from_slice(&digest.as_bytes()[..10]);
        JobId(bytes)
    }

    /// The job id of one deployment instance: `(deployment, generation,
    /// slot)` hashed deterministically (C2 §6.2).
    ///
    /// Deterministic for the same reason [`RunId::derive`] is: two nodes
    /// that briefly both believe they are the elected reconciler for a
    /// deployment name the **same** instance job rather than two, and the
    /// executor's journaled-acceptance rule collapses a double dispatch to
    /// one running instance. `slot` is the replica index for a fixed-count
    /// deployment and the executor's own node id for a `system` one — the
    /// two identities a supervisor tracks — and `generation` is what makes
    /// a rollout produce a fresh id per replica so the old and new
    /// instances coexist while a batch rolls.
    /// The job id of one occurrence of a service schedule (N1 P5).
    ///
    /// Deterministic for the reason [`RunId::derive`] is: two nodes that
    /// both briefly believe they are the elected firer for the same
    /// occurrence name the **same job** rather than two, and the
    /// executor's journaled-acceptance rule collapses a double dispatch to
    /// one running job. That is the whole of the exactly-once story —
    /// converge, rather than prevent or detect, because converge is the
    /// only one of the three available across a partition.
    ///
    /// Keeps the sortable millisecond prefix, unlike
    /// [`Self::derive_instance`]: a scheduled job *has* an occurrence
    /// instant to anchor on, so it sorts into history at the time it was
    /// meant to run rather than the time some node noticed it was due.
    pub fn derive_occurrence(schedule: &str, occurrence_micros: u64) -> Self {
        let millis = occurrence_micros / 1000;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"mw-service-schedule-job-v1");
        hasher.update(&(schedule.len() as u64).to_le_bytes());
        hasher.update(schedule.as_bytes());
        hasher.update(&occurrence_micros.to_le_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        bytes[6..].copy_from_slice(&digest.as_bytes()[..10]);
        JobId(bytes)
    }

    pub fn derive_instance(deployment: &str, generation: u32, slot: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"mw-deployment-instance-v1");
        hasher.update(&(deployment.len() as u64).to_le_bytes());
        hasher.update(deployment.as_bytes());
        hasher.update(&generation.to_le_bytes());
        hasher.update(&(slot.len() as u64).to_le_bytes());
        hasher.update(slot);
        // Fully deterministic — every byte from the digest, no time
        // prefix. Unlike a scheduled run there is no occurrence instant to
        // anchor on, and a wall-clock prefix would make two reconcilers a
        // tick apart derive different ids and defeat the idempotency this
        // exists for. Instances are keyed lookups, not a time-ordered scan,
        // so the sortable prefix `generate`/`RunId::derive` keep buys
        // nothing here and correctness forbids it.
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.as_bytes()[..16]);
        JobId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("job-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("job-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "job-").map(JobId)
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JobId({})", self.short())
    }
}

impl std::str::FromStr for JobId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(JobId, 16);

// ---------------------------------------------------------------------------
// BundleId & ChunkHash: BLAKE3 digests (32 bytes), hex-encoded for display.
// ---------------------------------------------------------------------------

/// Content address of a bundle: the BLAKE3 hash of its canonical manifest
/// (MASTERPLAN §8.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundleId(pub [u8; 32]);

impl BundleId {
    /// The id of **no bundle at all**.
    ///
    /// A container service is an image reference and often carries no code
    /// of its own: `nginx:alpine` needs nothing from the mesh's content
    /// store, and requiring one meant inventing a folder to push before a
    /// deployment could name it. This is the value that says *there is
    /// nothing to fetch*.
    ///
    /// A sentinel rather than an `Option<BundleId>` because
    /// [`crate::content::ServiceVersion`] is signed and postcard encodes
    /// positionally — changing the field's *shape* would invalidate every
    /// signature ever made over one. Changing its *value* costs nothing.
    ///
    /// Thirty-two zero bytes is not reachable by accident: it is a BLAKE3
    /// digest, and nothing hashes to zero.
    pub const NONE: BundleId = BundleId([0; 32]);

    /// Is this the [`Self::NONE`] sentinel — an image with no bundle?
    pub fn is_none(&self) -> bool {
        self.0 == [0u8; 32]
    }

    pub fn to_display_string(&self) -> String {
        format!("bndl-{}", data_encoding::HEXLOWER.encode(&self.0))
    }

    pub fn short(&self) -> String {
        format!("bndl-{}", &data_encoding::HEXLOWER.encode(&self.0)[..12])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        let body = s
            .strip_prefix("bndl-")
            .ok_or_else(|| ProtoError::InvalidId(s.to_string()))?;
        let bytes = data_encoding::HEXLOWER
            .decode(body.to_lowercase().as_bytes())
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        Ok(BundleId(arr))
    }
}

impl std::fmt::Display for BundleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for BundleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BundleId({})", self.short())
    }
}

impl std::str::FromStr for BundleId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A content chunk's BLAKE3 hash. Serialized as raw bytes (many per manifest).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChunkHash(pub [u8; 32]);

impl ChunkHash {
    pub fn to_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.0)
    }
}

impl std::fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChunkHash({})", &self.to_hex()[..12])
    }
}

human_readable_serde!(BundleId, 32);

// ---------------------------------------------------------------------------
// InviteId
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InviteId(pub [u8; 8]);

impl InviteId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        InviteId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("inv-", &self.0)
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "inv-").map(InviteId)
    }
}

impl std::fmt::Display for InviteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for InviteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InviteId({})", self.to_display_string())
    }
}

impl std::str::FromStr for InviteId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(InviteId, 8);

// ---------------------------------------------------------------------------
// GangId: identifies one all-or-nothing reservation over a set of nodes
// (C4 §8.2). Same time-sortable layout as FanId, and for the same reason:
// a gang *is* the barrier-shaped sibling of a fan-out group — one logical
// submission whose per-member results are ordinary JobIds — so it reuses
// the id shape rather than inventing one.
//
// The time prefix is load-bearing here in a way it is not for a fan-out:
// a gang's seniority ticket breaks its final tie on the raw id bytes
// (`GangTicket`), so an id that sorts by submission time makes the last
// resort of the tiebreak agree with the first two instead of fighting them.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GangId(pub [u8; 16]);

impl GangId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        GangId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("gang-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("gang-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "gang-").map(GangId)
    }
}

impl std::fmt::Display for GangId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for GangId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GangId({})", self.short())
    }
}

impl std::str::FromStr for GangId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(GangId, 16);

// ---------------------------------------------------------------------------
// LeaseId: one node's soft reservation, handed back to whoever asked for it
// (C4 §8.2 P1).
//
// Deliberately *not* derived from the gang id. A lease is the node's own
// handle on its own resources, minted fresh on every accepted offer, so a
// retried offer for the same gang after a TTL expiry cannot be answered
// with a lease the node has already reclaimed — the id changing is how the
// two are told apart without either side keeping history.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaseId(pub [u8; 16]);

impl LeaseId {
    pub fn generate() -> Self {
        let millis = crate::now_micros() / 1000;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
        let _ = getrandom::fill(&mut bytes[6..]);
        LeaseId(bytes)
    }

    pub fn to_display_string(&self) -> String {
        encode_body("lease-", &self.0)
    }

    pub fn short(&self) -> String {
        let s = ID_ENCODING.encode(&self.0).to_lowercase();
        format!("lease-{}", &s[..10.min(s.len())])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        decode_body(s, "lease-").map(LeaseId)
    }
}

impl std::fmt::Display for LeaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for LeaseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LeaseId({})", self.short())
    }
}

impl std::str::FromStr for LeaseId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(LeaseId, 16);

// ---------------------------------------------------------------------------
// ObjectId
// ---------------------------------------------------------------------------

/// Content address of an **ephemeral object**: the BLAKE3 hash of its
/// canonical manifest (M9 §5.5).
///
/// Shaped exactly like [`BundleId`] because it addresses content the same
/// way — identical bytes anywhere in the mesh produce the same id, so a
/// re-executed task recreates the object it lost rather than needing it
/// replicated. What differs is lifetime and index, not addressing: an
/// object is refcounted to the job that made it and located through a
/// rendezvous-hashed directory, never through the op-log.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub fn to_display_string(&self) -> String {
        format!("obj-{}", data_encoding::HEXLOWER.encode(&self.0))
    }

    pub fn short(&self) -> String {
        format!("obj-{}", &data_encoding::HEXLOWER.encode(&self.0)[..12])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        let body = s
            .strip_prefix("obj-")
            .ok_or_else(|| ProtoError::InvalidId(s.to_string()))?;
        let bytes = data_encoding::HEXLOWER
            .decode(body.to_lowercase().as_bytes())
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        Ok(ObjectId(arr))
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl std::str::FromStr for ObjectId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(ObjectId, 32);

// ---------------------------------------------------------------------------
// ActionKey
// ---------------------------------------------------------------------------

/// A cache key: BLAKE3 over the canonical [`crate::cache::ActionSpec`]
/// (C0–C9 §11.2).
///
/// Here rather than beside the derivation, with the other typed ids,
/// because that is what it is — and because a generated client finds
/// its wire shape by reading `human_readable_serde!`, which lives in this
/// module. An id whose serde was hand-rolled somewhere else would be an id
/// such a client could not decode.
///
/// Hex, like [`ObjectId`] and unlike [`NodeId`]: both are content
/// addresses, both are routinely pasted into a shell, and hex is what every
/// other tool that prints a digest prints.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionKey(pub [u8; 32]);

impl ActionKey {
    pub fn to_display_string(&self) -> String {
        format!("act-{}", data_encoding::HEXLOWER.encode(&self.0))
    }

    pub fn short(&self) -> String {
        format!("act-{}", &data_encoding::HEXLOWER.encode(&self.0)[..12])
    }

    pub fn parse(s: &str) -> Result<Self, ProtoError> {
        let body = s
            .strip_prefix("act-")
            .ok_or_else(|| ProtoError::InvalidId(s.to_string()))?;
        let bytes = data_encoding::HEXLOWER
            .decode(body.to_lowercase().as_bytes())
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ProtoError::InvalidId(s.to_string()))?;
        Ok(ActionKey(arr))
    }
}

impl std::fmt::Display for ActionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

impl std::fmt::Debug for ActionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActionKey({})", self.short())
    }
}

impl std::str::FromStr for ActionKey {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

human_readable_serde!(ActionKey, 32);

/// Lowercase hex of a 32-byte digest — for digests a *human* compares,
/// where a 32-element JSON array is not something anyone looks at.
pub fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Inverse of [`hex32`]. `None` for anything that is not 64 hex digits.
pub fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod derived_id_tests {
    use super::*;

    #[test]
    fn a_runs_id_is_a_pure_function_of_its_occurrence() {
        let a = RunId::derive("nightly-upgrade", 1_700_000_000_000_000);
        let b = RunId::derive("nightly-upgrade", 1_700_000_000_000_000);
        assert_eq!(a, b, "two nodes must name the same run for one occurrence");

        // Everything that distinguishes an occurrence must distinguish the id.
        assert_ne!(a, RunId::derive("nightly-upgrade", 1_700_000_060_000_000));
        assert_ne!(a, RunId::derive("nightly-backup", 1_700_000_000_000_000));
    }

    #[test]
    fn a_derived_run_id_sorts_by_the_time_it_was_meant_to_run() {
        let earlier = RunId::derive("s", 1_700_000_000_000_000);
        let later = RunId::derive("s", 1_700_003_600_000_000);
        assert!(
            earlier < later,
            "the millisecond prefix has to keep runs time-ordered, or history reads backwards"
        );
    }

    #[test]
    fn step_ids_are_a_pure_function_of_the_run() {
        let run = RunId::derive("s", 1_700_000_000_000_000);
        assert_eq!(
            JobId::derive(&run, "drain", 0),
            JobId::derive(&run, "drain", 0)
        );
        // The three things that must never collide inside one run.
        assert_ne!(
            JobId::derive(&run, "drain", 0),
            JobId::derive(&run, "drain", 1)
        );
        assert_ne!(
            JobId::derive(&run, "drain", 0),
            JobId::derive(&run, "upgrade", 0)
        );
        let other = RunId::derive("s", 1_700_000_060_000_000);
        assert_ne!(
            JobId::derive(&run, "drain", 0),
            JobId::derive(&other, "drain", 0)
        );
        // A step's fan-out group is derived from the same inputs and must
        // still be its own value.
        assert_ne!(
            JobId::derive(&run, "drain", 0).0,
            FanId::derive(&run, "drain", 0).0,
            "the job and group id of one attempt must not be the same 16 bytes"
        );
    }

    #[test]
    fn a_runs_jobs_share_its_time_prefix() {
        let run = RunId::derive("s", 1_700_000_000_000_000);
        let job = JobId::derive(&run, "drain", 0);
        assert_eq!(&job.0[..6], &run.0[..6]);
        assert_eq!(&FanId::derive(&run, "drain", 0).0[..6], &run.0[..6]);
    }
}
