//! Verifying a meshnode release (M24 §9).
//!
//! One question, asked in four places: the node about to replace its own
//! binary, the CLI publishing a release, Director publishing one, and
//! Director listing builds it *could* publish. They must all answer it
//! identically — a build Director calls publishable and a node then
//! refuses is worse than either refusing alone — so the answer lives here
//! rather than in whichever crate asked first.
//!
//! The signature is minisign over the whole binary. The key is not a
//! parameter of the *transport*: it does not matter whether the bytes came
//! off a peer, off a release server, or off a USB stick, because the peer
//! serving them is a CDN and not an authority.

use anyhow::{Result, anyhow, bail};

/// Verify `bin` against a minisign `.minisig` document with one base64
/// public key. Prehashed (current minisign) and legacy signatures are both
/// accepted — both are Ed25519 signatures by the same key.
pub fn verify_minisign(bin: &[u8], minisig: &str, pubkey_b64: &str) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(pubkey_b64.trim())
        .map_err(|e| anyhow!("release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(minisig)
        .map_err(|e| anyhow!("release signature: {e}"))?;
    pk.verify(bin, &sig, true)
        .map_err(|e| anyhow!("signature verification FAILED: {e}"))
}

/// Verify against **any** of `pubkeys`, returning the one that matched.
///
/// A set rather than a key, because trust is mesh state and a fleet may be
/// mid-rotation: the outgoing key still verifies what is installed while
/// the incoming one signs what is being published. Accepting any of them
/// is no weaker than accepting one — putting a key in that set is itself
/// an admin-signed act.
///
/// The error counts the keys tried, because "verification failed" against
/// a fleet trusting three keys is a different problem from the same
/// sentence against one, and an operator cannot tell them apart otherwise.
pub fn verify_minisign_any(bin: &[u8], minisig: &str, pubkeys: &[String]) -> Result<String> {
    let mut last: Option<anyhow::Error> = None;
    for key in pubkeys {
        match verify_minisign(bin, minisig, key) {
            Ok(()) => return Ok(key.clone()),
            Err(e) => last = Some(e),
        }
    }
    match (pubkeys.len(), last) {
        (0, _) => bail!(
            "no release key to verify against — trust one (Director → Updates → Release keys, \
             or `meshnode upgrade --trust-key`)"
        ),
        (1, Some(e)) => Err(e),
        (n, Some(e)) => Err(anyhow!(
            "signature verification FAILED against all {n} keys this mesh trusts (last: {e}) — \
             the build was signed by a key nobody has been told about"
        )),
        (_, None) => bail!("signature verification FAILED"),
    }
}

// ---------------------------------------------------------------------------
// Signing: every mesh is its own release authority
// ---------------------------------------------------------------------------
//
// Meshware has no project-wide release key. Whoever runs a mesh decides
// which binaries it installs, so the key that vouches for a binary is one of
// that mesh's own admins' — never a key some upstream maintainer must keep
// safe, renew and never lose for every fleet in the world to keep working.
//
// An admin's release key is **derived from their user key**, not generated
// and stored beside it. That means nothing new to back up, lose or protect:
// it is sealed and unlocked exactly as `user.key` is, an identity backup
// carries it, and the same admin gets the same release key on any machine.
// It also costs no security: an admin's user key can already run any
// command on any node, so a release key that falls with it gives an
// attacker nothing they did not have.
//
// The signature is ordinary minisign (prehashed, `ED`): `minisign -V`
// checks it, and so does [`verify_minisign`].

/// The domain separator for deriving a release key from a user key. Fixed
/// for the life of the format: changing it changes every admin's key.
pub const RELEASE_KEY_CONTEXT: &str = "meshware 2026-11-13 release signing key v1";

/// A minisign signing key: an admin's release key.
pub struct ReleaseSigner {
    key: ed25519_dalek::SigningKey,
    keynum: [u8; 8],
}

impl std::fmt::Debug for ReleaseSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret half.
        f.debug_struct("ReleaseSigner")
            .field("public_key", &self.public_key())
            .finish()
    }
}

impl ReleaseSigner {
    /// The release key belonging to the identity whose 32-byte Ed25519 seed
    /// is `user_seed`. Deterministic, and one-way: the release key reveals
    /// nothing about the user key it came from.
    pub fn derive(user_seed: &[u8; 32]) -> Self {
        let seed = blake3::derive_key(RELEASE_KEY_CONTEXT, user_seed);
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        // minisign's key id is any 8 bytes that the public key and every
        // signature carry alike; taken from the public key so it is stable.
        let mut keynum = [0u8; 8];
        keynum.copy_from_slice(&blake3::hash(key.verifying_key().as_bytes()).as_bytes()[..8]);
        ReleaseSigner { key, keynum }
    }

    /// The public key as minisign writes it: the `RW…` line of a `.pub`.
    pub fn public_key(&self) -> String {
        let mut blob = Vec::with_capacity(42);
        blob.extend_from_slice(b"Ed");
        blob.extend_from_slice(&self.keynum);
        blob.extend_from_slice(self.key.verifying_key().as_bytes());
        data_encoding::BASE64.encode(&blob)
    }

    /// The key id minisign prints: the key number as a little-endian u64,
    /// in upper-case hex.
    pub fn key_id(&self) -> String {
        format!("{:016X}", u64::from_le_bytes(self.keynum))
    }

    /// A whole `minisign.pub` file, for `minisign -V -p` or for
    /// `[release] pubkey` on a machine that should pin it.
    pub fn public_key_file(&self) -> String {
        format!(
            "untrusted comment: minisign public key {}\n{}\n",
            self.key_id(),
            self.public_key()
        )
    }

    /// Sign `bin`. `trusted_comment` is covered by the signature, so it is
    /// a fact about the release a verifier can rely on; newlines are
    /// replaced, because minisign's format is line-based.
    pub fn sign(&self, bin: &[u8], trusted_comment: &str) -> String {
        use blake2::Digest as _;
        use ed25519_dalek::Signer as _;
        let trusted_comment = trusted_comment.replace(['\r', '\n'], " ");
        let digest = blake2::Blake2b512::digest(bin);
        let signature = self.key.sign(digest.as_slice()).to_bytes();
        let mut line = Vec::with_capacity(74);
        line.extend_from_slice(b"ED");
        line.extend_from_slice(&self.keynum);
        line.extend_from_slice(&signature);
        let mut global = Vec::with_capacity(64 + trusted_comment.len());
        global.extend_from_slice(&signature);
        global.extend_from_slice(trusted_comment.as_bytes());
        let global_signature = self.key.sign(&global).to_bytes();
        format!(
            "untrusted comment: signature from meshware release key {}\n{}\ntrusted comment: {}\n{}\n",
            self.key_id(),
            data_encoding::BASE64.encode(&line),
            trusted_comment,
            data_encoding::BASE64.encode(&global_signature)
        )
    }
}

/// Check `bin` against its line in a `SHA256SUMS` file (`sha256sum`'s
/// output: `<hex>  <name>`, or `<hex> *<name>` in binary mode).
///
/// What this proves, exactly: the bytes are the ones the server listed.
/// The list comes from the same server, so it catches a corrupt or
/// truncated download and nothing more — the vouching is done by whoever
/// then signs the binary with their own release key, having decided to
/// trust where it came from.
pub fn check_sha256sums(sums: &str, artifact: &str, bin: &[u8]) -> Result<()> {
    let want = sums
        .lines()
        .filter_map(|l| {
            let (hash, name) = l.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            (name == artifact).then(|| hash.to_ascii_lowercase())
        })
        .next()
        .ok_or_else(|| anyhow!("{artifact} is not listed in SHA256SUMS"))?;
    let got =
        data_encoding::HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, bin).as_ref());
    if got != want {
        bail!(
            "{artifact} does not match SHA256SUMS (got {got}, listed {want}) — a corrupt or altered download"
        );
    }
    Ok(())
}

/// The trusted comment a mesh-signed release carries.
pub fn release_trusted_comment(version: &str, artifact: &str, at_unix: u64) -> String {
    format!("timestamp:{at_unix}\tfile:{artifact}\tmeshware release {version}")
}

// ---------------------------------------------------------------------------
// One version, several architectures
// ---------------------------------------------------------------------------
//
// A release record names one exact binary, and a binary is for one
// architecture. A fleet is not: a rack of x86_64 servers and a shelf of
// aarch64 boards are one mesh, and "roll 0.7.0 to the outdated ring" has
// to mean the right file for each. So the replicated map may hold one
// record per architecture for a version. The **first** record keeps the
// bare version as its key — an older node that only ever looks up
// `"0.7.0"` finds exactly what it found before — and each further
// architecture lands under `version@arch`. Every reader that knows about
// architectures asks [`for_arch`], which scans by version and matches the
// artifact name, so which record happens to hold the bare key does not
// matter on any node that runs this code.

use std::collections::BTreeMap;

use mesh_proto::ReleaseRecord;

/// The architecture a release record is for, read off its artifact name.
pub fn arch_of(rec: &ReleaseRecord) -> Option<&str> {
    crate::deploy::arch_of_artifact(&rec.artifact)
}

/// The map key a new record lands under.
///
/// The bare version when nothing holds it yet, or when what holds it is
/// the same architecture — that second case is a duplicate, and the
/// caller's `or_insert` keeps the first, which is the "first publish of a
/// version wins" rule unchanged. A *different* architecture gets its own
/// key.
pub fn release_key(existing: &BTreeMap<String, ReleaseRecord>, rec: &ReleaseRecord) -> String {
    match existing.get(&rec.version) {
        None => rec.version.clone(),
        Some(first) if arch_of(first) == arch_of(rec) => rec.version.clone(),
        Some(_) => match arch_of(rec) {
            Some(arch) => format!("{}@{arch}", rec.version),
            None => rec.version.clone(),
        },
    }
}

/// Every record published for `version`, whichever key each sits under.
pub fn records_for<'a>(
    releases: &'a BTreeMap<String, ReleaseRecord>,
    version: &str,
) -> impl Iterator<Item = &'a ReleaseRecord> {
    let version = version.trim().trim_start_matches(['v', 'V']).to_string();
    releases.values().filter(move |r| r.version == version)
}

/// The record a node of architecture `arch` should install for `version`.
///
/// A record whose artifact carries no recognisable architecture is
/// accepted as a last resort — it predates this naming, and the upgrade
/// process still runs `--version` on it before installing.
pub fn for_arch<'a>(
    releases: &'a BTreeMap<String, ReleaseRecord>,
    version: &str,
    arch: &str,
) -> Option<&'a ReleaseRecord> {
    let want = crate::deploy::normalise_arch(arch);
    let mut fallback = None;
    for rec in records_for(releases, version) {
        match arch_of(rec) {
            Some(a) if a == want => return Some(rec),
            None => fallback = Some(rec),
            Some(_) => {}
        }
    }
    fallback
}

/// The architectures `version` is published for.
pub fn arches_for(releases: &BTreeMap<String, ReleaseRecord>, version: &str) -> Vec<String> {
    let mut out: Vec<String> = records_for(releases, version)
        .filter_map(|r| arch_of(r).map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod arch_tests {
    use super::*;

    fn rec(version: &str, arch: &str) -> ReleaseRecord {
        ReleaseRecord {
            version: version.into(),
            bundle_id: mesh_proto::BundleId([7; 32]),
            artifact: crate::deploy::artifact_name(version, arch),
            minisig: String::new(),
            digest: [0; 32],
            published_by: mesh_proto::UserId([1; 32]),
            published_at_micros: 0,
            signature: Vec::new(),
        }
    }

    #[test]
    fn a_second_architecture_gets_its_own_key_and_the_first_keeps_the_bare_one() {
        let mut map = BTreeMap::new();
        let x = rec("0.7.0", "x86_64");
        let key = release_key(&map, &x);
        assert_eq!(key, "0.7.0");
        map.insert(key, x);

        // Same arch again: same key, so `or_insert` keeps the first.
        assert_eq!(release_key(&map, &rec("0.7.0", "x86_64")), "0.7.0");

        let a = rec("0.7.0", "aarch64");
        let key = release_key(&map, &a);
        assert_eq!(key, "0.7.0@aarch64");
        map.insert(key, a);

        assert_eq!(
            for_arch(&map, "v0.7.0", "arm64").map(|r| r.artifact.as_str()),
            Some("meshnode-v0.7.0-aarch64-unknown-linux-musl")
        );
        assert_eq!(
            for_arch(&map, "0.7.0", "amd64").map(|r| r.artifact.as_str()),
            Some("meshnode-v0.7.0-x86_64-unknown-linux-musl")
        );
        assert!(for_arch(&map, "0.7.0", "riscv64").is_none());
        assert_eq!(arches_for(&map, "0.7.0"), ["aarch64", "x86_64"]);
        assert!(arches_for(&map, "0.8.0").is_empty());
    }

    // -- signing ------------------------------------------------------------

    const SEED: [u8; 32] = [7u8; 32];

    #[test]
    fn a_signed_release_verifies_against_its_public_key() {
        let signer = ReleaseSigner::derive(&SEED);
        let bin = b"\x7fELF pretend this is meshnode".repeat(1000);
        let sig = signer.sign(&bin, &release_trusted_comment("1.2.3", "meshnode-x", 1));
        verify_minisign(&bin, &sig, &signer.public_key()).expect("verifies");
        assert_eq!(
            verify_minisign_any(&bin, &sig, &["RWQnope".into(), signer.public_key()]).unwrap(),
            signer.public_key()
        );
    }

    #[test]
    fn a_changed_binary_or_another_key_does_not_verify() {
        let signer = ReleaseSigner::derive(&SEED);
        let other = ReleaseSigner::derive(&[8u8; 32]);
        let sig = signer.sign(b"the real one", "c");
        assert!(verify_minisign(b"the real onE", &sig, &signer.public_key()).is_err());
        assert!(verify_minisign(b"the real one", &sig, &other.public_key()).is_err());
    }

    #[test]
    fn the_trusted_comment_is_covered() {
        let signer = ReleaseSigner::derive(&SEED);
        let sig = signer.sign(b"bin", "meshware release 1.0.0");
        let forged = sig.replace("meshware release 1.0.0", "meshware release 9.9.9");
        assert!(verify_minisign(b"bin", &forged, &signer.public_key()).is_err());
        // and a newline cannot smuggle a fifth line in
        let sig = signer.sign(b"bin", "one\ntwo");
        assert_eq!(sig.lines().count(), 4);
        verify_minisign(b"bin", &sig, &signer.public_key()).expect("still verifies");
    }

    #[test]
    fn the_release_key_is_stable_and_distinct_from_the_user_key() {
        let a = ReleaseSigner::derive(&SEED);
        let b = ReleaseSigner::derive(&SEED);
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(a.key_id().len(), 16);
        let user_pk = ed25519_dalek::SigningKey::from_bytes(&SEED).verifying_key();
        assert_ne!(
            a.key.verifying_key(),
            user_pk,
            "derived, never the user key itself"
        );
        assert!(mesh_proto::ReleaseKey::looks_like_minisign(&a.public_key()));
        assert!(
            a.public_key_file()
                .starts_with("untrusted comment: minisign public key ")
        );
        let shown = format!("{a:?}");
        assert!(
            shown.contains(&a.public_key()) && !shown.contains("SigningKey"),
            "Debug shows the public key and never the secret: {shown}"
        );
    }

    /// The real `minisign` agrees, where it is installed: a signature only
    /// our own verifier accepts would be a format of our own.
    #[test]
    fn minisign_itself_accepts_the_signature() {
        let Ok(out) = std::process::Command::new("minisign").arg("-v").output() else {
            eprintln!("minisign not installed; skipping");
            return;
        };
        if !out.status.success() && out.stdout.is_empty() && out.stderr.is_empty() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let signer = ReleaseSigner::derive(&SEED);
        let bin = dir.path().join("meshnode");
        std::fs::write(&bin, b"a release").unwrap();
        std::fs::write(
            dir.path().join("meshnode.minisig"),
            signer.sign(b"a release", "tc"),
        )
        .unwrap();
        std::fs::write(dir.path().join("k.pub"), signer.public_key_file()).unwrap();
        let ok = std::process::Command::new("minisign")
            .args(["-V", "-H", "-q", "-m"])
            .arg(&bin)
            .arg("-p")
            .arg(dir.path().join("k.pub"))
            .status()
            .unwrap();
        assert!(ok.success(), "minisign -V -H rejected our signature");
    }

    #[test]
    fn sha256sums_are_matched_by_name_in_either_mode() {
        let bin = b"meshnode bytes";
        let hex = data_encoding::HEXLOWER
            .encode(ring::digest::digest(&ring::digest::SHA256, bin).as_ref());
        let sums = format!("{hex}  other\n{hex} *meshnode-v1-x86_64-unknown-linux-musl\n");
        check_sha256sums(&sums, "meshnode-v1-x86_64-unknown-linux-musl", bin).expect("listed");
        assert!(
            check_sha256sums(&sums, "meshnode-v1-x86_64-unknown-linux-musl", b"tampered").is_err()
        );
        assert!(check_sha256sums(&sums, "not-there", bin).is_err());
    }
}
