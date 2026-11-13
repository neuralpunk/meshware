//! Verifying a signature a human made by touching a key (Y6).
//!
//! ## The attack this exists to stop
//!
//! `SECURITY.md` states that a node verifies every binary against the
//! minisign release key and that the check cannot be turned off. That is
//! good design, and it has a consequence: **whoever holds that key can push
//! a binary to every fleet running meshware.** It is the one secret in the
//! project whose loss ends the project, and today it is a file that CI can
//! read.
//!
//! minisign has no hardware-token support, so the key cannot simply be
//! moved. What can be done is to require a *second* signature that a file
//! cannot produce: `ssh-keygen -Y sign` with a key on a token. A stolen CI
//! secret then buys an attacker a minisign signature and nothing else,
//! because the second one needs a device in somebody's hand.
//!
//! ## Why the SSHSIG format, and not something new
//!
//! It is what `ssh-keygen -Y sign` already emits, which means the signing
//! side of this feature is a command every developer already has, on
//! hardware they may already own — no new tool to trust with the most
//! valuable key in the project. It is domain-separated by a `namespace`
//! string, so a signature made for meshware cannot be a signature made for
//! a git commit. And it is a handful of length-prefixed fields, so
//! verifying one needs no library at all.
//!
//! ## The two key types, and why `sk-` is the interesting one
//!
//! - `ssh-ed25519` — an ordinary Ed25519 key. On a YubiHSM or in a PIV
//!   slot reached through an agent, this is a hardware key; in a file it is
//!   not, and nothing in the signature says which. Accepted because a
//!   YubiHSM 2 holding the key on an air-gapped machine is a perfectly good
//!   answer, and because the *key* being trusted is an admin act either
//!   way.
//! - `sk-ssh-ed25519@openssh.com` — a FIDO2 resident credential, from
//!   `ssh-keygen -t ed25519-sk`. The signature carries the authenticator's
//!   flags, and [`SshSignature::user_present`] reads the bit that says a
//!   human physically touched the device. **That bit is the feature**: it
//!   is the difference between "this release was signed" and "a person was
//!   present when this release was signed", and a key stolen from a CI
//!   secret store can never set it.
//!
//! ## Tested against the real thing
//!
//! The unit tests carry a signature `ssh-keygen -Y sign` actually produced,
//! byte for byte, rather than one built by a fixture in this file. Y5's
//! presence bug was sixteen green tests in which the fixture signed exactly
//! what the verifier checked: both agreed, and both were wrong about CTAP.
//! A fixture can only ever prove two pieces of your own code agree.

use ring::signature;

use crate::error::{CoreError, Result};
use crate::hardware::{Reader, put_string};

/// The domain every meshware release signature is made under.
///
/// `ssh-keygen -Y sign -n meshware-release`. Fixed and checked, because a
/// namespace is the whole of SSHSIG's domain separation: without it, a
/// signature somebody made over a git commit with the same key would verify
/// here.
pub const RELEASE_NAMESPACE: &str = "meshware-release";

const MAGIC: &[u8; 6] = b"SSHSIG";
const SIG_VERSION: u32 = 1;
const ED25519: &str = "ssh-ed25519";
const SK_ED25519: &str = "sk-ssh-ed25519@openssh.com";

/// Refuse anything bigger than this. An armored signature is a few hundred
/// bytes; a megabyte of it is somebody probing.
const MAX_ARMORED: usize = 16 * 1024;

/// The authenticator flag that says a human touched the device.
const FLAG_USER_PRESENT: u8 = 0x01;

/// One parsed `-----BEGIN SSH SIGNATURE-----` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSignature {
    /// The whole public-key blob, as the signature carries it. Compared
    /// byte for byte against a trusted key; never rebuilt from parts.
    pub public_key_blob: Vec<u8>,
    /// `ssh-ed25519` or `sk-ssh-ed25519@openssh.com`.
    pub algorithm: String,
    pub namespace: String,
    /// `sha256` or `sha512`, as the signer chose.
    pub hash_algorithm: String,
    /// Raw 64-byte Ed25519 signature.
    pub signature: [u8; 64],
    /// Present only for `sk-` keys: the authenticator's flags and counter.
    pub sk: Option<SkTail>,
}

/// What a FIDO2 authenticator added to the signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkTail {
    pub flags: u8,
    pub counter: u32,
}

impl SshSignature {
    /// The public key as an `authorized_keys` line would spell it:
    /// `ssh-ed25519 AAAA…`. This is what a mesh stores and trusts.
    pub fn public_key_line(&self) -> String {
        format!(
            "{} {}",
            self.algorithm,
            data_encoding::BASE64.encode(&self.public_key_blob)
        )
    }

    /// Was a human physically at the device?
    ///
    /// `false` for an ordinary key, which cannot answer the question at
    /// all — and "cannot answer" must never read as "yes".
    pub fn user_present(&self) -> bool {
        self.sk.is_some_and(|t| t.flags & FLAG_USER_PRESENT != 0)
    }

    /// Is this a key a *token* holds, rather than one that might be a file?
    pub fn is_token_backed(&self) -> bool {
        self.sk.is_some()
    }
}

/// Parse an armored SSHSIG document.
pub fn parse(armored: &str) -> Result<SshSignature> {
    if armored.len() > MAX_ARMORED {
        return Err(CoreError::Identity(format!(
            "that signature is {} bytes; an SSH signature is a few hundred",
            armored.len()
        )));
    }
    let body: String = armored
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("-----"))
        .collect();
    let raw = data_encoding::BASE64
        .decode(body.as_bytes())
        .map_err(|e| CoreError::Identity(format!("signature is not base64: {e}")))?;

    let Some(rest) = raw.strip_prefix(MAGIC.as_slice()) else {
        return Err(CoreError::Identity(
            "not an SSH signature: it does not start with SSHSIG. `ssh-keygen -Y sign` writes \
             one"
            .into(),
        ));
    };
    let mut r = Reader::new(rest);
    let version = r.u32()?;
    if version != SIG_VERSION {
        return Err(CoreError::Identity(format!(
            "SSH signature version {version}; this build understands {SIG_VERSION}"
        )));
    }
    let public_key_blob = r.string()?.to_vec();
    let namespace = String::from_utf8_lossy(r.string()?).into_owned();
    let _reserved = r.string()?;
    let hash_algorithm = String::from_utf8_lossy(r.string()?).into_owned();
    let sig_blob = r.string()?;

    // The algorithm is read from the *key* blob, not from the signature
    // blob, and then the two are required to agree below. A signature that
    // names one algorithm over a key that is another is exactly the shape
    // of a downgrade.
    let mut kb = Reader::new(&public_key_blob);
    let algorithm = String::from_utf8_lossy(kb.string()?).into_owned();

    let mut sb = Reader::new(sig_blob);
    let sig_alg = String::from_utf8_lossy(sb.string()?).into_owned();
    if sig_alg != algorithm {
        return Err(CoreError::Identity(format!(
            "the signature says {sig_alg} and the key says {algorithm}"
        )));
    }
    let sig_bytes = sb.string()?;
    let signature = <[u8; 64]>::try_from(sig_bytes).map_err(|_| {
        CoreError::Identity(format!(
            "a {}-byte Ed25519 signature is not one",
            sig_bytes.len()
        ))
    })?;
    let sk = match algorithm.as_str() {
        ED25519 => None,
        SK_ED25519 => {
            let flags = sb.byte()?;
            let counter = sb.u32()?;
            Some(SkTail { flags, counter })
        }
        other => {
            return Err(CoreError::Identity(format!(
                "meshware release signatures are Ed25519; that key is {other}"
            )));
        }
    };

    Ok(SshSignature {
        public_key_blob,
        algorithm,
        namespace,
        hash_algorithm,
        signature,
        sk,
    })
}

/// Does this signature cover `message`, under `namespace`, with its own key?
///
/// The caller decides whether that key is *trusted* — this only answers
/// whether the signature is real, exactly as `release::verify_minisign`
/// splits those two questions.
pub fn verify(sig: &SshSignature, message: &[u8], namespace: &str) -> Result<()> {
    if sig.namespace != namespace {
        return Err(CoreError::Identity(format!(
            "that signature was made for {:?}, not for {namespace:?} — a signature over one \
             thing is not a signature over another",
            sig.namespace
        )));
    }
    let hashed = match sig.hash_algorithm.as_str() {
        "sha256" => ring::digest::digest(&ring::digest::SHA256, message)
            .as_ref()
            .to_vec(),
        "sha512" => ring::digest::digest(&ring::digest::SHA512, message)
            .as_ref()
            .to_vec(),
        other => {
            return Err(CoreError::Identity(format!(
                "unknown hash algorithm {other:?} in the signature"
            )));
        }
    };

    // The blob the signer actually signed, rebuilt here rather than taken
    // from the document. Nothing in a signature gets to say what it covers.
    let mut blob = Vec::with_capacity(128 + hashed.len());
    blob.extend_from_slice(MAGIC);
    put_string(&mut blob, namespace.as_bytes());
    put_string(&mut blob, b"");
    put_string(&mut blob, sig.hash_algorithm.as_bytes());
    put_string(&mut blob, &hashed);

    let mut kb = Reader::new(&sig.public_key_blob);
    let _alg = kb.string()?;
    let key = kb.string()?;

    let message_to_verify = match sig.sk {
        None => blob,
        Some(tail) => {
            // A FIDO2 key signs the authenticator's own framing:
            // SHA256(application) ‖ flags ‖ counter ‖ SHA256(blob). The
            // application comes from the *key*, so a signature cannot
            // claim to have been made for a different origin.
            let application = kb.string()?;
            let mut m = Vec::with_capacity(69);
            m.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, application).as_ref());
            m.push(tail.flags);
            m.extend_from_slice(&tail.counter.to_be_bytes());
            m.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &blob).as_ref());
            m
        }
    };

    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(&message_to_verify, &sig.signature)
        .map_err(|_| {
            CoreError::Identity(
                "that SSH signature does not cover these bytes with that key".into(),
            )
        })
}

/// Is `line` an SSH public key rather than a minisign one? (Y6)
///
/// Used to tell the two kinds apart inside one trusted-key list, which is
/// why this feature adds no record type: a minisign key line starts `RW`,
/// an SSH one names its algorithm first, and the two can never be confused
/// for each other.
pub fn is_ssh_key_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("ssh-ed25519 ") || line.starts_with("sk-ssh-ed25519@openssh.com ")
}

/// Does this signature's key match a trusted `authorized_keys`-style line?
///
/// Compared on the algorithm and the base64 blob only: a key line's trailing
/// comment is whatever somebody typed, and requiring it to match would mean
/// a mesh that stops trusting a key because a label changed.
pub fn key_line_matches(sig: &SshSignature, trusted: &str) -> bool {
    let mut parts = trusted.split_whitespace();
    let (Some(alg), Some(blob)) = (parts.next(), parts.next()) else {
        return false;
    };
    alg == sig.algorithm && blob == data_encoding::BASE64.encode(&sig.public_key_blob)
}

/// Which trusted key made this co-signature, and was a human there? (Y6)
///
/// `None` when the document does not parse, or when no trusted key made it.
/// One function so the merge path and the pre-publish check cannot disagree
/// about what "trusted" means — the same reason `release.rs` exists.
///
/// The bytes signed are **not** passed in: a co-signature is verified
/// against a trusted key here, and against the binary itself by
/// [`verify_release_cosignature`]. Splitting them means a mesh can record a
/// co-signature it cannot yet check the binary of, which is exactly what
/// happens when one is published before its release is fetched.
pub fn match_trusted(armored: &str, trusted: &[String]) -> Option<(String, bool)> {
    let sig = parse(armored).ok()?;
    if sig.namespace != RELEASE_NAMESPACE {
        return None;
    }
    trusted
        .iter()
        .find(|line| key_line_matches(&sig, line))
        .map(|line| (line.clone(), sig.user_present()))
}

/// Does a recorded co-signature actually cover this binary? (Y6)
///
/// The half `match_trusted` deliberately leaves out. A co-signature by a
/// trusted key over *a different binary* is a valid signature and an
/// invalid release, and only a node holding the bytes can tell.
pub fn verify_release_cosignature(armored: &str, bin: &[u8], trusted: &[String]) -> Result<String> {
    let sig = parse(armored)?;
    let Some(line) = trusted.iter().find(|line| key_line_matches(&sig, line)) else {
        return Err(CoreError::Identity(
            "that co-signature was made by a key this mesh does not trust for releases — \
             `meshnode releases keys add`"
                .into(),
        ));
    };
    verify(&sig, bin, RELEASE_NAMESPACE)?;
    Ok(line.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A signature `ssh-keygen -Y sign` actually made**, over exactly the
    /// message below, with the key below. Not a fixture built by this file.
    const REAL_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgOYX+nApb0Wtu+BNpzM5ctXC/TR
J5cvMtq5RTK3ilrr4AAAAQbWVzaHdhcmUtcmVsZWFzZQAAAAAAAAAGc2hhNTEyAAAAUwAA
AAtzc2gtZWQyNTUxOQAAAEApYrIdyXMSB5fw7iwK0ajsTdMVtZ6wuzWxpTx1QRywkKUycC
Q1eRQJw01dNk0A0MrfyJSMsTBJmA3V6Frbf0wH
-----END SSH SIGNATURE-----";
    const REAL_MESSAGE: &[u8] = b"meshware release 0.7.3 test payload";
    const REAL_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDmF/pwKW9FrbvgTaczOXLVwv00SeXLzLauUUyt4pa6+ meshware release test";

    #[test]
    fn a_real_ssh_keygen_signature_verifies() {
        let sig = parse(REAL_SIG).expect("parse");
        assert_eq!(sig.algorithm, ED25519);
        assert_eq!(sig.namespace, RELEASE_NAMESPACE);
        assert_eq!(sig.hash_algorithm, "sha512");
        verify(&sig, REAL_MESSAGE, RELEASE_NAMESPACE).expect("a real signature must verify");
        assert!(key_line_matches(&sig, REAL_KEY));
    }

    /// The whole point: a signature over one release is not a signature
    /// over another.
    #[test]
    fn one_byte_of_the_message_is_enough_to_break_it() {
        let sig = parse(REAL_SIG).expect("parse");
        let mut tampered = REAL_MESSAGE.to_vec();
        tampered[0] ^= 0x01;
        assert!(verify(&sig, &tampered, RELEASE_NAMESPACE).is_err());
    }

    /// Domain separation. Without the namespace check, a signature this
    /// key made over a git commit would verify a release binary.
    #[test]
    fn a_signature_from_another_namespace_is_refused() {
        let sig = parse(REAL_SIG).expect("parse");
        let err = verify(&sig, REAL_MESSAGE, "git").expect_err("must refuse");
        assert!(
            err.to_string().contains("meshware-release"),
            "the refusal names what it was actually for: {err}"
        );
    }

    /// An ordinary key cannot say a human was there, and "cannot say" must
    /// never read as "yes" — that bit is the entire difference between a
    /// CI-stealable signature and one that is not.
    #[test]
    fn an_ordinary_key_never_claims_a_human_was_present() {
        let sig = parse(REAL_SIG).expect("parse");
        assert!(!sig.is_token_backed());
        assert!(
            !sig.user_present(),
            "an ssh-ed25519 key has no authenticator and no presence bit"
        );
    }

    #[test]
    fn a_minisign_key_line_is_not_mistaken_for_an_ssh_one() {
        assert!(is_ssh_key_line(REAL_KEY));
        assert!(is_ssh_key_line(
            "sk-ssh-ed25519@openssh.com AAAAG3NrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29t comment"
        ));
        assert!(!is_ssh_key_line(
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
        ));
        assert!(!is_ssh_key_line(""));
    }

    /// Garbage in is a sentence out, never a panic: these bytes arrive in
    /// an op from another node.
    #[test]
    fn nonsense_is_refused_rather_than_parsed() {
        assert!(parse("").is_err());
        assert!(
            parse("-----BEGIN SSH SIGNATURE-----\nnot base64!!\n-----END SSH SIGNATURE-----")
                .is_err()
        );
        // Valid base64, wrong magic.
        assert!(
            parse(&format!(
                "-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----",
                data_encoding::BASE64.encode(b"NOTSSHSIG and then some")
            ))
            .is_err()
        );
        assert!(parse(&"A".repeat(MAX_ARMORED + 1)).is_err());
    }

    /// A key line's trailing comment is whatever somebody typed. Matching
    /// on it would mean a mesh that stops trusting a key because a label
    /// changed.
    #[test]
    fn a_trusted_key_matches_regardless_of_its_comment() {
        let sig = parse(REAL_SIG).expect("parse");
        let no_comment: String = REAL_KEY
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(key_line_matches(&sig, &no_comment));
        assert!(key_line_matches(
            &sig,
            &format!("{no_comment} someone@elsewhere")
        ));
        assert!(!key_line_matches(
            &sig,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
    }
}
