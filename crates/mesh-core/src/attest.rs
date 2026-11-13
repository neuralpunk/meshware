//! Verifying that a key was made on hardware (Y2/Y3).
//!
//! The daemon half of [`mesh_proto::attest`]. Pure Rust: `x509-parser` is
//! already in this crate's graph for the transport's peer-certificate
//! check, and it verifies signatures through the same `ring` everything
//! else here does.
//!
//! ## The three things a chain has to establish
//!
//! Each is necessary and the order is chosen so a refusal names the cause a
//! person can act on, exactly as [`crate::presence`] does.
//!
//! 1. **The leaf is about this key.** The certificate's subject public key
//!    must be the Ed25519 key of the user claiming it. Without this an
//!    attacker attaches somebody else's genuine YubiKey attestation to
//!    their own software key and the column says "hardware".
//! 2. **The chain links.** Each certificate is signed by the next, and the
//!    last is signed by a root this mesh trusts.
//! 3. **The root is trusted here.** Trust is replicated state an admin
//!    wrote, not a certificate compiled into a binary.
//!
//! ## What a refusal is, and is not
//!
//! A failed attestation is not a failed signature and must never read like
//! one. The user's identity is unaffected: they keep their key, their
//! grants and their access. What they do not get is the column. An
//! attestation is a claim *about* a key, and a claim that does not check
//! out is simply not recorded.
//!
//! ## Expiry is deliberately not checked
//!
//! A YubiKey PIV attestation leaf is valid for the life of the *key*, and
//! Yubico's intermediates outlive most fleets — but a certificate that
//! expires does not make the key it attested suddenly exportable. What the
//! chain proves is a fact about the past: this key was generated inside
//! this device. Refusing it later would mean a mesh that silently drops a
//! security property on a date nobody chose, which is the same failure mode
//! as a scheduler that degrades without saying so.
//!
//! The dates are *reported* — `meshnode users --hardware` prints them — so
//! an operator who wants to rotate on expiry can, and nothing decides it
//! for them.

use mesh_proto::UserId;
use mesh_proto::attest::HardwareAttestation;

/// Why an attestation was not accepted. Each names one cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestRefusal {
    /// Empty, too long, or a certificate too big to be one.
    Malformed,
    /// A certificate in the chain is not parseable X.509.
    UnreadableCertificate(usize),
    /// The leaf certifies a different public key than the claimant's.
    WrongKey,
    /// Two certificates in the chain do not link.
    BrokenChain(usize),
    /// The chain ends at nothing this mesh trusts.
    UntrustedRoot,
    /// The mesh trusts no attestation roots at all, so it can verify
    /// nothing. Named separately because the fix is completely different.
    NoRootsConfigured,
}

impl AttestRefusal {
    pub fn message(&self) -> String {
        match self {
            AttestRefusal::Malformed => {
                "that attestation is not a well-formed chain (empty, too long, or a certificate \
                 larger than 8 KiB)"
                    .into()
            }
            AttestRefusal::UnreadableCertificate(i) => {
                format!("certificate {i} in the chain is not readable X.509 DER")
            }
            AttestRefusal::WrongKey => {
                "that certificate attests a different key from the one signing this claim — an \
                 attestation of somebody else's key says nothing about yours"
                    .into()
            }
            AttestRefusal::BrokenChain(i) => format!(
                "certificate {i} is not signed by the one after it; the chain has to run leaf \
                 first, then intermediates"
            ),
            AttestRefusal::UntrustedRoot => {
                "that chain does not end at a certificate authority this mesh trusts — \
                 `meshnode users attest-root add <file>` to trust one"
                    .into()
            }
            AttestRefusal::NoRootsConfigured => {
                "this mesh trusts no attestation authorities, so it cannot verify any chain. An \
                 admin adds one with `meshnode users attest-root add <file>`"
                    .into()
            }
        }
    }
}

/// What an accepted chain establishes, for a list and for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attested {
    pub user_id: UserId,
    /// The trusted root that vouched for it, by label.
    pub root_label: String,
    /// The leaf's subject, verbatim. On a YubiKey PIV attestation this
    /// carries the device serial; on a private CA it carries whatever that
    /// CA puts there. Recorded for a person to read, never parsed.
    pub subject: String,
    /// Leaf validity, as the certificate states it. Reported, not enforced
    /// — see the module header.
    pub not_before_micros: u64,
    pub not_after_micros: u64,
}

/// Is this a chain from a key the claimant holds to a root this mesh trusts?
///
/// `roots` is `(id, label, der)` for every root the mesh currently trusts;
/// the caller supplies it because only the caller knows replicated state.
pub fn verify(
    attestation: &HardwareAttestation,
    roots: &[(String, String, Vec<u8>)],
) -> std::result::Result<Attested, AttestRefusal> {
    use x509_parser::prelude::*;

    if !attestation.well_formed() {
        return Err(AttestRefusal::Malformed);
    }
    if roots.is_empty() {
        return Err(AttestRefusal::NoRootsConfigured);
    }

    // Parse the whole chain first, so a broken certificate is reported as
    // one rather than as a link failure two steps later.
    let mut parsed = Vec::with_capacity(attestation.chain.len());
    for (i, der) in attestation.chain.iter().enumerate() {
        let (_, cert) =
            X509Certificate::from_der(der).map_err(|_| AttestRefusal::UnreadableCertificate(i))?;
        parsed.push(cert);
    }

    // 1. The leaf has to be about the key that signed the claim. This is
    //    the check that stops a genuine attestation for somebody else's
    //    YubiKey being stapled to a software key.
    let leaf = &parsed[0];
    let spki = leaf.public_key();
    if spki.algorithm.algorithm != x509_parser::oid_registry::OID_SIG_ED25519 {
        return Err(AttestRefusal::WrongKey);
    }
    let key: &[u8] = spki.subject_public_key.data.as_ref();
    if key != attestation.user_id.0 {
        return Err(AttestRefusal::WrongKey);
    }

    // 2. Each certificate is signed by the next one along.
    for i in 0..parsed.len().saturating_sub(1) {
        if parsed[i]
            .verify_signature(Some(parsed[i + 1].public_key()))
            .is_err()
        {
            return Err(AttestRefusal::BrokenChain(i));
        }
    }

    // 3. ...and the last is signed by a root this mesh trusts. The root's
    //    own self-signature is not checked: a root is trusted because an
    //    admin said so in an op that carries their signature, and asking a
    //    self-signed certificate to vouch for itself adds nothing.
    let last = parsed.last().ok_or(AttestRefusal::Malformed)?;
    for (_, label, der) in roots {
        let Ok((_, root)) = X509Certificate::from_der(der) else {
            // A root that will not parse is a bad op somebody merged, not
            // a reason to refuse a chain that might verify against another.
            continue;
        };
        // A chain whose last element *is* the root verifies trivially; one
        // that ends at an intermediate has to be signed by it.
        let linked = last.tbs_certificate.as_ref() == root.tbs_certificate.as_ref()
            || last.verify_signature(Some(root.public_key())).is_ok();
        if linked {
            return Ok(Attested {
                user_id: attestation.user_id,
                root_label: label.clone(),
                subject: leaf.subject().to_string(),
                not_before_micros: to_micros(leaf.validity().not_before.timestamp()),
                not_after_micros: to_micros(leaf.validity().not_after.timestamp()),
            });
        }
    }
    Err(AttestRefusal::UntrustedRoot)
}

/// Seconds since the epoch as the microseconds the rest of the tree uses.
///
/// Saturating rather than wrapping: a certificate dated before 1970 is
/// nonsense, and `0` reads as "unknown" everywhere a timestamp is printed.
fn to_micros(secs: i64) -> u64 {
    u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::attest::AttestKind;

    fn claim(chain: Vec<Vec<u8>>) -> HardwareAttestation {
        HardwareAttestation {
            user_id: UserId([1u8; 32]),
            kind: AttestKind::Piv,
            chain,
            slot: "9c".into(),
            added_at_micros: 1,
            signature: Vec::new(),
        }
    }

    /// A mesh that trusts nothing says so, rather than saying the chain is
    /// bad. The fix is an admin act, and a refusal that sends somebody to
    /// re-generate a key instead is a wasted afternoon.
    #[test]
    fn no_trusted_roots_is_its_own_refusal() {
        let err = verify(&claim(vec![vec![0u8; 64]]), &[]).expect_err("must refuse");
        assert_eq!(err, AttestRefusal::NoRootsConfigured);
        assert!(
            err.message().contains("attest-root add"),
            "{}",
            err.message()
        );
    }

    /// The shape gate runs before any parsing, so a hostile op never
    /// reaches the X.509 code.
    #[test]
    fn a_malformed_chain_is_refused_before_it_is_parsed() {
        let roots = vec![("id".into(), "root".into(), vec![0u8; 8])];
        assert_eq!(
            verify(&claim(Vec::new()), &roots),
            Err(AttestRefusal::Malformed)
        );
        assert_eq!(
            verify(
                &claim(vec![vec![0u8; mesh_proto::attest::MAX_CERT_BYTES + 1]]),
                &roots
            ),
            Err(AttestRefusal::Malformed)
        );
    }

    /// Bytes that are not a certificate are reported as that, with the
    /// position, rather than as an untrusted root.
    #[test]
    fn unparseable_bytes_name_the_certificate_that_failed() {
        let roots = vec![("id".into(), "root".into(), vec![0u8; 8])];
        assert_eq!(
            verify(&claim(vec![vec![0xFFu8; 64]]), &roots),
            Err(AttestRefusal::UnreadableCertificate(0))
        );
        assert_eq!(
            verify(&claim(vec![vec![0xFFu8; 64], vec![0xAAu8; 64]]), &roots),
            Err(AttestRefusal::UnreadableCertificate(0)),
            "the first bad one is the one to name"
        );
    }

    // -- against certificates that are actually certificates ---------------
    //
    // `rcgen` is already in this crate's graph for the transport's
    // self-signed node certificate, so these chains are real X.509 signed
    // by real keys rather than fixtures agreeing with the verifier. That
    // distinction is not academic here: Y5's presence bug was sixteen green
    // tests where the fixture signed exactly what the verifier checked.

    struct Ca {
        params: rcgen::CertificateParams,
        key: rcgen::KeyPair,
        der: Vec<u8>,
    }

    fn ca(name: &str) -> Ca {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("ca key");
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("params");
        params.distinguished_name = {
            let mut dn = rcgen::DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, name);
            dn
        };
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let der = params
            .clone()
            .self_signed(&key)
            .expect("self-signed ca")
            .der()
            .to_vec();
        Ca { params, key, der }
    }

    /// A leaf certifying `subject`'s public key, signed by `ca` — the shape
    /// a PIV attestation has: the device's CA vouching for a key generated
    /// inside it.
    fn leaf(ca: &Ca, subject: &rcgen::KeyPair, cn: &str) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("params");
        params.distinguished_name = {
            let mut dn = rcgen::DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, cn);
            dn
        };
        let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
        params
            .signed_by(subject, &issuer)
            .expect("leaf")
            .der()
            .to_vec()
    }

    fn user_of(key: &rcgen::KeyPair) -> UserId {
        let raw: [u8; 32] = key
            .public_key_raw()
            .try_into()
            .expect("ed25519 public keys are 32 bytes");
        UserId(raw)
    }

    #[test]
    fn a_chain_to_a_trusted_root_for_this_key_is_accepted() {
        let authority = ca("Test PIV Attestation CA");
        let subject = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("key");
        let mut a = claim(vec![leaf(
            &authority,
            &subject,
            "YubiKey PIV Attestation 9c",
        )]);
        a.user_id = user_of(&subject);

        let roots = vec![(
            "id".to_string(),
            "Test PIV Attestation CA".to_string(),
            authority.der.clone(),
        )];
        let attested = verify(&a, &roots).expect("a real chain must verify");
        assert_eq!(attested.user_id, a.user_id);
        assert_eq!(attested.root_label, "Test PIV Attestation CA");
        assert!(
            attested.subject.contains("YubiKey PIV Attestation 9c"),
            "the leaf's subject carries the device detail: {}",
            attested.subject
        );
    }

    /// **The attack this check exists to stop.** A genuine attestation for
    /// somebody else's YubiKey, stapled to your own software key, would
    /// otherwise put HARDWARE beside a key in a file.
    #[test]
    fn a_genuine_chain_for_a_different_key_is_refused() {
        let authority = ca("Test PIV Attestation CA");
        let theirs = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("key");
        // The chain is real and the root is trusted. Only the claimant is
        // wrong — and that is enough.
        let a = claim(vec![leaf(&authority, &theirs, "somebody else's key")]);
        let roots = vec![(
            "id".to_string(),
            "Test PIV Attestation CA".to_string(),
            authority.der.clone(),
        )];
        assert_eq!(verify(&a, &roots), Err(AttestRefusal::WrongKey));
    }

    /// A perfectly valid chain from an authority nobody here trusts proves
    /// nothing here. This is what makes the replicated root list the actual
    /// trust decision rather than decoration.
    #[test]
    fn a_chain_from_an_untrusted_authority_is_refused() {
        let mine = ca("My CA");
        let theirs = ca("Some Other CA");
        let subject = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("key");
        let mut a = claim(vec![leaf(&theirs, &subject, "a key")]);
        a.user_id = user_of(&subject);

        let roots = vec![("id".to_string(), "My CA".to_string(), mine.der.clone())];
        assert_eq!(verify(&a, &roots), Err(AttestRefusal::UntrustedRoot));
    }

    /// Two certificates that do not link are reported as a broken chain at
    /// the position where it breaks, not as an untrusted root — the fixes
    /// are completely different.
    #[test]
    fn a_chain_whose_links_do_not_join_names_the_break() {
        let authority = ca("Test CA");
        let unrelated = ca("Unrelated CA");
        let subject = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("key");
        let mut a = claim(vec![
            leaf(&authority, &subject, "a key"),
            // An intermediate that did not sign the leaf.
            unrelated.der.clone(),
        ]);
        a.user_id = user_of(&subject);

        let roots = vec![(
            "id".to_string(),
            "Unrelated CA".to_string(),
            unrelated.der.clone(),
        )];
        assert_eq!(verify(&a, &roots), Err(AttestRefusal::BrokenChain(0)));
    }

    /// Trusting an intermediate directly has to work: an organisation adds
    /// its issuing CA, not the manufacturer's root above it.
    #[test]
    fn a_chain_ending_at_the_trusted_certificate_itself_verifies() {
        let authority = ca("Issuing CA");
        let subject = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("key");
        let mut a = claim(vec![
            leaf(&authority, &subject, "a key"),
            authority.der.clone(),
        ]);
        a.user_id = user_of(&subject);

        let roots = vec![(
            "id".to_string(),
            "Issuing CA".to_string(),
            authority.der.clone(),
        )];
        assert!(verify(&a, &roots).is_ok());
    }

    /// Every refusal has to be a sentence somebody can act on — the raw
    /// alternative is a position in a byte string.
    #[test]
    fn every_refusal_says_what_to_do() {
        for r in [
            AttestRefusal::Malformed,
            AttestRefusal::UnreadableCertificate(1),
            AttestRefusal::WrongKey,
            AttestRefusal::BrokenChain(0),
            AttestRefusal::UntrustedRoot,
            AttestRefusal::NoRootsConfigured,
        ] {
            let m = r.message();
            assert!(!m.is_empty());
            assert!(
                m.chars().next().is_some_and(|c| c.is_lowercase()),
                "refusals read as clauses, like presence.rs: {m}"
            );
        }
    }
}
