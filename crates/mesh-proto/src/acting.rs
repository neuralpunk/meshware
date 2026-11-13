//! Acting for the person whose job you are running.
//!
//! A task that calls back into the mesh has to sign, and it cannot sign as
//! the person who submitted it: their key is on their machine and stays
//! there. So the node running the job mints a keypair for that job and
//! remembers, locally, that it speaks for the submitter
//! (`mesh_core::jobs::JobManager::mint_job_credential`).
//!
//! That is enough on the origin, which minted it. It is not enough anywhere
//! else, and the difference is the whole of this module.
//!
//! ## What a remote executor is entitled to
//!
//! Every node re-verifies work before running it (MASTERPLAN §7.2). For a
//! fan-out that means three checks, and they are strong on purpose:
//! the envelope is signed by `submitter_user`; the signature covers *this
//! exact command over these exact items*; and that user has a grant
//! covering this node. Together they say: **a compromised origin cannot run
//! work here as a user who never asked for it.**
//!
//! A per-job credential breaks the first check and only the first. The
//! second and third are untouched — the envelope still covers exactly the
//! work being offered, and the user's grants are still evaluated against
//! the executor's own state. What the executor loses is the ability to tell
//! that the signing key really speaks for that user, because the binding
//! lived in one node's memory.
//!
//! ## So the origin says so, in writing
//!
//! [`ActingFor`] is that binding, signed by the origin's **node** key —
//! which every member already knows from replicated state and already
//! verifies for everything else a node says. It rides the reserved
//! `MESHWARE_ACT_*` env keys rather than growing `JobSpec` or
//! `ParallelPlan`, for the reason M9 §5.3 records: those are decoded by
//! every peer, and an env map is not.
//!
//! ## What that does and does not buy
//!
//! It converts an unverifiable claim into an attributable one. A remote
//! executor can now check that a **named, live member** asserts this
//! binding, and refuse if the node is unknown, revoked or lying about its
//! own identity. A forged attestation needs a member's node private key.
//!
//! It does not make the assertion *true*. A compromised member that is
//! running your job can attest a binding for you and fan work out as you,
//! for as long as it holds a job of yours. That is a real extension of
//! trust and it is stated here rather than buried: **a node you give work
//! to can act as you while it holds that work.** It is bounded by the job's
//! lifetime, attributed to a named node in every audit row, and revoked the
//! moment that node is — and it is the price of "the steps of my workflow
//! are mine", which cannot be paid any other way without putting the
//! submitter's private key on somebody else's machine.
//!
//! ## The bound, actually enforced (S-M8)
//!
//! "Bounded by the job's lifetime" was a claim this module made and nothing
//! checked. The signed statement covered `(key, user, job, node)` and no
//! time at all, and the verifier never asked whether the cited job was still
//! running — so the real bound was "as long as the attesting node remains a
//! member", which can be years. The per-job key lives in the job's own
//! directory, readable by the job.
//!
//! Two things close it, and a verifier applies both:
//!
//!   * the statement carries `expires_at_micros`, inside the signature —
//!     [`ACT_TTL_MICROS`] from minting — so it stops standing on its own;
//!   * the verifier refuses a claim whose job it *knows* has finished. S-M8
//!     accepted a job it had not heard of, because fan-out routinely
//!     outruns the summary describing it. That acceptance was the hole
//!     SEC-1 closed below: since 1.30 an unknown job costs one pull from
//!     the vouching node, and is then refused.
//!
//! The deadline is why [`ActingFor::signing_bytes`] has more than one
//! domain. A claim with no deadline is an origin running an older build; it
//! verifies under `v1` and is bounded by the job alone.
//!
//! ## A claim has to prove the user asked (SEC-1, protocol 1.30)
//!
//! S-M8 bounded the claim in time and left it unproven in substance. The
//! verifier took a *job id it had never heard of* as good enough, and never
//! asked whether the claiming node had run anything for that user at all.
//! A job summary would not have settled it either: summaries are authored
//! by the executor they name, and nothing checks the `submitter_user` in
//! one. So any member — a borrowed laptop, somebody's Director — could sign
//! "this key speaks for the founder", cite a job id it made up, and run
//! commands as the founder on every node.
//!
//! A `v3` claim carries an [`ActingProof`]: the **user-signed envelope**
//! that authorized the job being cited, and, when that job was itself
//! submitted by a job credential, the claim *it* carried — so a chain
//! bottoms out in a real signature by the real user, never in a node's
//! say-so. The proof's digest is inside the node's signature. A verifier
//! (`mesh_core::daemon`) checks each link against its own state:
//!
//!   * the cited job is known, is the user's, was executed by the claiming
//!     node, and had not finished when the link was used;
//!   * the envelope is the user's (or an attested credential's, one level
//!     down), its signature verifies, and it dates from the job;
//!   * an envelope that the mesh replicates — a fan-out or a gang plan
//!     carries it to every member — proves only the targets that plan
//!     names, so holding a copy of it is not the same as being given the
//!     work;
//!   * a deployment instance or a scheduled job has no user envelope to
//!     show (its authority is a replicated record every member can read),
//!     so it cannot act for its creator on another node. A service account
//!     is the identity for automation that has to.
//!
//! What remains is the extension of trust this module states above, now
//! actually bounded: a node you give work to can act as you while it holds
//! that work, for at most [`ACT_PROOF_MAX_AGE_MICROS`] after you signed it.
//!
//! `v1` and `v2` claims are **refused** by a 1.30 verifier. A v3 claim still
//! carries a `v2` signature under [`ACT_SIG`], so a pre-1.30 verifier —
//! which is exactly as open as it was before and gains nothing from
//! refusing — keeps working through a rolling upgrade. The proof and its
//! signature ride their own keys, [`ACT_PROOF`] and [`ACT_PROOF_SIG`].

use serde::{Deserialize, Serialize};

use crate::{JobId, NodeId, SignedIpc, UserId};

/// Reserved env prefix for the attestation. Reserved so it cannot arrive
/// from a workflow definition or a `--env` flag — not that a forged one
/// would verify, but a key that decides authorization has no business
/// being settable by the thing being authorized.
pub const ACT_ENV_PREFIX: &str = "MESHWARE_ACT_";
/// `mw1…` — the member asserting the binding.
pub const ACT_NODE: &str = "MESHWARE_ACT_NODE";
/// `job-…` — the job it was running when it did.
pub const ACT_JOB: &str = "MESHWARE_ACT_JOB";
/// Hex Ed25519 signature by that node over [`ActingFor::signing_bytes`].
pub const ACT_SIG: &str = "MESHWARE_ACT_SIG";
/// Decimal micros — when this statement stops being true (S-M8).
///
/// Appended rather than folded into an existing key, and *optional on the
/// way in*: an origin running a build from before this existed emits no such
/// key, and its claim is verified under the v1 signing bytes. It gains
/// nothing from the deadline and loses nothing either — the job-liveness
/// check applies to both.
pub const ACT_UNTIL: &str = "MESHWARE_ACT_UNTIL";

/// How long a minted attestation stands (S-M8).
///
/// Long enough that no legitimate fan-out, gang or workflow step is cut off
/// mid-flight by it, and short enough that a leaked per-job key is not a
/// standing licence. The real bound is the job's lifetime — this is the
/// backstop for the case where the job's terminal summary never reaches the
/// node doing the verifying.
pub const ACT_TTL_MICROS: u64 = 24 * 60 * 60 * 1_000_000;

/// Base64 postcard [`ActingProof`] — what the user signed (SEC-1).
pub const ACT_PROOF: &str = "MESHWARE_ACT_PROOF";
/// Hex Ed25519 signature by the node over the `v3` signing bytes, which
/// cover the proof. [`ACT_SIG`] keeps the `v2` signature beside it for
/// verifiers that predate the proof.
pub const ACT_PROOF_SIG: &str = "MESHWARE_ACT_PROOF_SIG";

/// How long after the user signed a request a claim built on it can still
/// stand (SEC-1).
///
/// The job's lifetime is the bound an honest node keeps. This is the bound
/// on a dishonest one: an executor controls its own job summaries and could
/// otherwise report a job of yours as running forever. A week is longer
/// than any job that should be calling back into the mesh as a person —
/// automation that runs longer belongs under a service account.
pub const ACT_PROOF_MAX_AGE_MICROS: u64 = 7 * 24 * 60 * 60 * 1_000_000;

/// How many credentials deep a chain may go: a job that submits a job that
/// submits a job… Each level adds one envelope to the proof, so this is
/// also what bounds the proof's size.
pub const ACT_MAX_DEPTH: usize = 4;

/// What a claim offers as evidence that the user really asked (SEC-1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActingProof {
    /// The authorization of the job the claim cites — `JobSpec::auth`
    /// exactly as the claiming node accepted it.
    pub envelope: SignedIpc,
    /// That job's own `MESHWARE_ACT_*` pairs, when `envelope` was signed by
    /// a job credential rather than by the user. Empty otherwise.
    pub inner: Vec<(String, String)>,
}

impl ActingProof {
    /// The claim one level down: the attestation that `envelope`'s signer
    /// spoke for `user`. `None` when there is none, or it does not decode.
    pub fn inner_claim(&self, user: UserId) -> Option<(ActingFor, Vec<u8>)> {
        ActingFor::from_env(&self.inner, self.envelope.user, user)
    }

    /// Links in this chain, counting this one.
    pub fn depth(&self) -> usize {
        let mut depth = 1;
        let mut inner = self.inner.clone();
        while let Some(proof) = proof_from_env(&inner) {
            depth += 1;
            if depth > ACT_MAX_DEPTH {
                break;
            }
            inner = proof.inner;
        }
        depth
    }

    fn digest(&self) -> [u8; 32] {
        let bytes = postcard::to_allocvec(self).unwrap_or_default();
        *blake3::hash(&bytes).as_bytes()
    }
}

/// The proof carried in `env`, if any decodes.
fn proof_from_env(env: &[(String, String)]) -> Option<ActingProof> {
    let raw = env.iter().find(|(k, _)| k == ACT_PROOF)?.1.trim();
    let bytes = data_encoding::BASE64.decode(raw.as_bytes()).ok()?;
    postcard::from_bytes(&bytes).ok()
}

/// The `MESHWARE_ACT_*` pairs of an environment, as a proof's `inner`.
pub fn act_pairs(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter(|(k, _)| k.starts_with(ACT_ENV_PREFIX))
        .cloned()
        .collect()
}

/// One node's statement that a key speaks for a user, under one job.
#[derive(Debug, Clone, PartialEq)]
pub struct ActingFor {
    /// The per-job credential's public key — the envelope's signer.
    pub key: UserId,
    /// Who it speaks for.
    pub user: UserId,
    /// The job it was minted for.
    pub job: JobId,
    /// The member making the claim.
    pub node: NodeId,
    /// When this stops being true, in micros (S-M8). `None` is a claim from
    /// an origin that predates the field; it verifies under the v1 signing
    /// bytes and carries no deadline of its own.
    pub expires_at_micros: Option<u64>,
    /// What the user signed (SEC-1). `None` is a pre-1.30 claim, which a
    /// 1.30 verifier refuses.
    pub proof: Option<ActingProof>,
}

impl ActingFor {
    /// Domain-separated, and it covers every field: a signature that did
    /// not name the job would be a standing licence to act as that user,
    /// and one that did not name the node would be replayable by any other.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + 32 + 32 + 16 + 32 + 8 + 32);
        // One domain per shape, because the deadline and the proof are
        // inside the signature and a signature that did not cover them
        // would let anyone drop them (S-M8, SEC-1). A verifier picks the
        // domain from what the claim carries, so neither can be stripped
        // to fall back to a weaker one.
        match (&self.proof, self.expires_at_micros) {
            (Some(_), _) => buf.extend_from_slice(b"mw-acting-for-v3"),
            (None, Some(_)) => buf.extend_from_slice(b"mw-acting-for-v2"),
            (None, None) => buf.extend_from_slice(b"mw-acting-for-v1"),
        }
        buf.extend_from_slice(&self.key.0);
        buf.extend_from_slice(&self.user.0);
        buf.extend_from_slice(&self.job.0);
        buf.extend_from_slice(&self.node.0);
        match (&self.proof, self.expires_at_micros) {
            (Some(proof), until) => {
                buf.extend_from_slice(&until.unwrap_or(0).to_le_bytes());
                buf.extend_from_slice(&proof.digest());
            }
            (None, Some(until)) => buf.extend_from_slice(&until.to_le_bytes()),
            (None, None) => {}
        }
        buf
    }

    /// The same claim without its proof — what a pre-1.30 verifier checks
    /// [`ACT_SIG`] against.
    fn legacy(&self) -> ActingFor {
        ActingFor {
            proof: None,
            ..self.clone()
        }
    }

    /// Has this statement passed its own deadline? A claim with no deadline
    /// (a pre-S-M8 origin) is never expired by this — it is bounded by the
    /// job instead.
    pub fn expired_at(&self, now_micros: u64) -> bool {
        self.expires_at_micros
            .is_some_and(|until| now_micros > until)
    }

    /// The env pairs that carry this, signed by `sign` — the claiming
    /// node's key.
    ///
    /// A claim with a proof is signed twice: under `v3` for verifiers that
    /// check the proof, and under `v2` (as [`ACT_SIG`]) for the ones that
    /// predate it and would otherwise refuse a signature they cannot read.
    pub fn to_env(&self, sign: impl Fn(&[u8]) -> [u8; 64]) -> Vec<(String, String)> {
        let legacy = self.legacy();
        let mut out = vec![
            (ACT_NODE.to_string(), self.node.to_display_string()),
            (ACT_JOB.to_string(), self.job.to_display_string()),
            (
                ACT_SIG.to_string(),
                data_encoding::HEXLOWER.encode(&sign(&legacy.signing_bytes())),
            ),
        ];
        if let Some(until) = self.expires_at_micros {
            out.push((ACT_UNTIL.to_string(), until.to_string()));
        }
        if let Some(proof) = &self.proof {
            let bytes = postcard::to_allocvec(proof).unwrap_or_default();
            out.push((ACT_PROOF.to_string(), data_encoding::BASE64.encode(&bytes)));
            out.push((
                ACT_PROOF_SIG.to_string(),
                data_encoding::HEXLOWER.encode(&sign(&self.signing_bytes())),
            ));
        }
        out
    }

    /// Recover a claim and its signature from a job or plan environment.
    ///
    /// `key` and `user` come from the envelope and the spec rather than
    /// from the environment: they are what the caller is *claiming*, and
    /// this returns the statement that has to be true for that claim to
    /// stand. Reading them from the env instead would let a forger sign a
    /// binding nobody asked about.
    pub fn from_env(
        env: &[(String, String)],
        key: UserId,
        user: UserId,
    ) -> Option<(Self, Vec<u8>)> {
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.trim())
                .filter(|v| !v.is_empty())
        };
        let node = NodeId::parse(get(ACT_NODE)?).ok()?;
        let job = JobId::parse(get(ACT_JOB)?).ok()?;
        // Absent means a pre-S-M8 origin, and the v1 signing bytes.
        // Present-but-unparseable is a claim somebody edited, so it decodes
        // as a *v2* claim with a deadline of zero and fails both checks
        // rather than quietly degrading to v1.
        let expires_at_micros = get(ACT_UNTIL).map(|v| v.parse::<u64>().unwrap_or(0));
        // A proof, when there is one, comes with its own signature, and that
        // is the one returned: the v2 signature beside it is for verifiers
        // that cannot read a proof. A proof that does not decode leaves a
        // v2 claim, which a 1.30 verifier refuses by name.
        let proof = get(ACT_PROOF).and_then(|_| proof_from_env(env));
        let signature = match (&proof, get(ACT_PROOF_SIG)) {
            (Some(_), Some(sig)) => data_encoding::HEXLOWER.decode(sig.as_bytes()).ok()?,
            (Some(_), None) => return None,
            (None, _) => data_encoding::HEXLOWER
                .decode(get(ACT_SIG)?.as_bytes())
                .ok()?,
        };
        Some((
            ActingFor {
                key,
                user,
                job,
                node,
                expires_at_micros,
                proof,
            },
            signature,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(user: UserId) -> SignedIpc {
        SignedIpc {
            user,
            at_micros: 500_000,
            nonce: 7,
            payload: vec![1, 2, 3],
            signature: vec![8u8; 64],
        }
    }

    fn proof() -> ActingProof {
        ActingProof {
            envelope: envelope(UserId([2u8; 32])),
            inner: Vec::new(),
        }
    }

    fn claim() -> ActingFor {
        ActingFor {
            key: UserId([1u8; 32]),
            user: UserId([2u8; 32]),
            job: JobId([3u8; 16]),
            node: NodeId([4u8; 32]),
            expires_at_micros: Some(1_000_000),
            proof: Some(proof()),
        }
    }

    /// A stand-in signer: the "signature" is a digest of what it covers, so
    /// a test can tell which bytes each key was signed over.
    fn sign(bytes: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(blake3::hash(bytes).as_bytes());
        out
    }

    #[test]
    fn every_field_is_covered_by_the_signature() {
        let base = claim().signing_bytes();
        for mutate in [
            |c: &mut ActingFor| c.key = UserId([9u8; 32]),
            |c: &mut ActingFor| c.user = UserId([9u8; 32]),
            |c: &mut ActingFor| c.job = JobId([9u8; 16]),
            |c: &mut ActingFor| c.node = NodeId([9u8; 32]),
            // S-M8: the deadline is inside the signature, so it cannot be
            // moved — and dropping it entirely changes the domain, so it
            // cannot be stripped either.
            |c: &mut ActingFor| c.expires_at_micros = Some(9_000_000),
            |c: &mut ActingFor| c.expires_at_micros = None,
            // SEC-1: so is the proof, and so is every part of it.
            |c: &mut ActingFor| c.proof = None,
            |c: &mut ActingFor| {
                c.proof.as_mut().expect("proof").envelope.at_micros = 1;
            },
            |c: &mut ActingFor| {
                c.proof.as_mut().expect("proof").envelope.signature = vec![9u8; 64];
            },
            |c: &mut ActingFor| {
                c.proof
                    .as_mut()
                    .expect("proof")
                    .inner
                    .push((ACT_NODE.into(), "x".into()));
            },
        ] {
            let mut other = claim();
            mutate(&mut other);
            assert_ne!(
                base,
                other.signing_bytes(),
                "a field outside the signature is a field an attacker may choose"
            );
        }
    }

    #[test]
    fn the_env_round_trips() {
        let c = claim();
        let env = c.to_env(sign);
        let (back, back_sig) = ActingFor::from_env(&env, c.key, c.user).expect("recovered");
        assert_eq!(back, c);
        // The signature handed back is the one over the proof.
        assert_eq!(back_sig, sign(&c.signing_bytes()).to_vec());
    }

    /// SEC-1: a v3 claim still carries a v2 signature, so a verifier that
    /// predates the proof reads exactly the claim it always did.
    #[test]
    fn a_pre_proof_verifier_still_reads_a_v3_claim() {
        let c = claim();
        let mut env = c.to_env(sign);
        let legacy_sig = env
            .iter()
            .find(|(k, _)| k == ACT_SIG)
            .expect("a v2 signature")
            .1
            .clone();
        let v2 = ActingFor {
            proof: None,
            ..c.clone()
        };
        assert_eq!(
            data_encoding::HEXLOWER
                .decode(legacy_sig.as_bytes())
                .expect("hex"),
            sign(&v2.signing_bytes()).to_vec()
        );
        assert!(v2.signing_bytes().starts_with(b"mw-acting-for-v2"));
        assert!(c.signing_bytes().starts_with(b"mw-acting-for-v3"));

        // What an old verifier sees: the keys it knows, and nothing else.
        env.retain(|(k, _)| k != ACT_PROOF && k != ACT_PROOF_SIG);
        let (back, back_sig) = ActingFor::from_env(&env, c.key, c.user).expect("recovered");
        assert_eq!(back, v2);
        assert_eq!(back_sig, sign(&v2.signing_bytes()).to_vec());
    }

    /// A proof without its signature is not quietly read as a v2 claim.
    #[test]
    fn a_proof_without_its_signature_is_none() {
        let c = claim();
        let mut env = c.to_env(sign);
        env.retain(|(k, _)| k != ACT_PROOF_SIG);
        assert!(ActingFor::from_env(&env, c.key, c.user).is_none());
    }

    /// A proof that does not decode leaves a proofless claim, which a 1.30
    /// verifier refuses — it never becomes a different, valid proof.
    #[test]
    fn a_broken_proof_leaves_no_proof() {
        let c = claim();
        let mut env = c.to_env(sign);
        for pair in &mut env {
            if pair.0 == ACT_PROOF {
                pair.1 = "not base64 at all".into();
            }
        }
        let (back, _) = ActingFor::from_env(&env, c.key, c.user).expect("decoded");
        assert!(back.proof.is_none());
    }

    /// S-M8: the statement says when it stops being true, and a claim from
    /// an origin that predates the field still decodes — under v1, with no
    /// deadline of its own.
    #[test]
    fn a_deadline_is_carried_and_enforced() {
        let c = claim();
        assert!(!c.expired_at(999_999));
        assert!(!c.expired_at(1_000_000));
        assert!(c.expired_at(1_000_001));

        let env = c.to_env(sign);
        assert!(env.iter().any(|(k, v)| k == ACT_UNTIL && v == "1000000"));
        let (back, _) = ActingFor::from_env(&env, c.key, c.user).expect("recovered");
        assert_eq!(back.expires_at_micros, Some(1_000_000));

        // A legacy claim: no key, no deadline, v1 bytes.
        let legacy = ActingFor {
            expires_at_micros: None,
            proof: None,
            ..claim()
        };
        let env = legacy.to_env(sign);
        assert!(env.iter().all(|(k, _)| k != ACT_UNTIL));
        let (back, _) = ActingFor::from_env(&env, legacy.key, legacy.user).expect("recovered");
        assert_eq!(back, legacy);
        assert!(!back.expired_at(u64::MAX));
        assert!(legacy.signing_bytes().starts_with(b"mw-acting-for-v1"));

        // A deadline somebody edited into nonsense is a claim with a
        // deadline of zero: expired, and signed over different bytes.
        let mut broken = claim().to_env(sign);
        for pair in &mut broken {
            if pair.0 == ACT_UNTIL {
                pair.1 = "tomorrow".into();
            }
        }
        let (back, _) = ActingFor::from_env(&broken, claim().key, claim().user).expect("decoded");
        assert_eq!(back.expires_at_micros, Some(0));
        assert!(back.expired_at(1));
    }

    #[test]
    fn an_absent_or_broken_claim_is_none() {
        assert!(ActingFor::from_env(&[], UserId([1; 32]), UserId([2; 32])).is_none());
        let mut env = claim().to_env(sign);
        env.retain(|(k, _)| k != ACT_SIG && k != ACT_PROOF && k != ACT_PROOF_SIG);
        assert!(ActingFor::from_env(&env, UserId([1; 32]), UserId([2; 32])).is_none());
    }

    /// A chain: the job's own claim rides inside the proof, and reads back
    /// as the claim one level down, about the envelope's signer.
    #[test]
    fn a_chain_nests_and_counts() {
        let user = UserId([2u8; 32]);
        let first_key = UserId([5u8; 32]);
        let inner_claim = ActingFor {
            key: first_key,
            user,
            job: JobId([6u8; 16]),
            node: NodeId([6u8; 32]),
            expires_at_micros: Some(2_000_000),
            proof: Some(proof()),
        };
        let outer = ActingProof {
            envelope: envelope(first_key),
            inner: inner_claim.to_env(sign),
        };
        assert_eq!(outer.depth(), 2);
        assert_eq!(proof().depth(), 1);
        let (back, sig) = outer.inner_claim(user).expect("inner claim");
        assert_eq!(back, inner_claim);
        assert_eq!(sig, sign(&inner_claim.signing_bytes()).to_vec());

        // Deeper than the cap stops counting just past it, so a caller can
        // refuse without walking an arbitrarily long chain.
        let mut proof = outer;
        for n in 0..10u8 {
            let claim = ActingFor {
                key: UserId([10 + n; 32]),
                user,
                job: JobId([n; 16]),
                node: NodeId([n; 32]),
                expires_at_micros: Some(1),
                proof: Some(proof),
            };
            proof = ActingProof {
                envelope: envelope(claim.key),
                inner: claim.to_env(sign),
            };
        }
        assert_eq!(proof.depth(), ACT_MAX_DEPTH + 1);
    }

    #[test]
    fn act_pairs_takes_only_the_claim() {
        let mut env = claim().to_env(sign);
        env.push(("PATH".into(), "/bin".into()));
        let pairs = act_pairs(&env);
        assert!(pairs.iter().all(|(k, _)| k.starts_with(ACT_ENV_PREFIX)));
        assert_eq!(pairs.len(), env.len() - 1);
    }
}
