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
}
