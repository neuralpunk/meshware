//! Stand up a **local, signed release channel** so a fleet upgrade can be
//! exercised without publishing anything (M24 §4).
//!
//! ```console
//! $ cargo run -p meshware-node --example dev-release-channel -- \
//!       v0.5.1 target/release/meshnode /tmp/mw-channel
//! ```
//!
//! It lays out exactly what `meshnode upgrade` fetches —
//!
//! ```text
//! <out>/latest                                     "v0.5.1"
//! <out>/v0.5.1/meshnode-v0.5.1-<arch>-unknown-linux-musl
//! <out>/v0.5.1/meshnode-v0.5.1-<arch>-unknown-linux-musl.minisig
//! ```
//!
//! — signs the binary with a dev key it generates once and keeps in the
//! output directory, and prints the `[release]` block to paste into each
//! node's `meshnode.toml`. Serve the directory over HTTP and the whole
//! rollout path is real: fetch, verify, swap, drain, restart, re-announce.
//!
//! **This is a development tool and it says so out loud.** The key it makes
//! is not the project's release key, it lives unencrypted next to the
//! artifacts it signs, and a mesh pointed at a channel like this trusts
//! whoever can write to that directory. Real releases are built and signed
//! by `packaging/build-release.sh` with a key that never touches a build
//! host.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signer, SigningKey};
use meshware_node::upgrade::{artifact_name, normalize_tag};

/// Minisign key ids exist so a verifier can say "wrong key" rather than
/// "bad signature". Fixed here: a dev channel has exactly one key, and a
/// stable id means a re-run does not invalidate what nodes already trust.
const DEV_KEY_ID: [u8; 8] = *b"mwdevch0";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [version, binary, out_dir] = args.as_slice() else {
        bail!(
            "usage: dev-release-channel <version> <path/to/meshnode> <out-dir>\n\
             \n\
             example:\n  cargo run -p meshware-node --example dev-release-channel -- \\\n\
             \x20   v0.5.1 target/release/meshnode /tmp/mw-channel"
        );
    };
    let tag = normalize_tag(version);
    let out_dir = PathBuf::from(out_dir);
    let binary = PathBuf::from(binary);

    let bytes = std::fs::read(&binary).with_context(|| format!("reading {}", binary.display()))?;

    // The upgrader runs `--version` on what it downloaded and refuses
    // anything that does not print the tag it asked for, so catching a
    // mismatch here saves a confusing failure on every node at once.
    let printed = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", binary.display()))?;
    let printed = String::from_utf8_lossy(&printed.stdout).trim().to_string();
    let bare = tag.trim_start_matches('v');
    if !printed.contains(bare) {
        bail!(
            "{} reports {printed:?}, which does not contain {bare} — every node would download \
             it, refuse it, and leave itself untouched. Build the binary at that version first.",
            binary.display()
        );
    }

    let key = load_or_generate_key(&out_dir)?;
    let artifact = artifact_name(&tag);
    let dir = out_dir.join(&tag);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join(&artifact), &bytes)?;
    std::fs::write(
        dir.join(format!("{artifact}.minisig")),
        sign_legacy(&key, &bytes, &format!("dev channel artifact {artifact}")),
    )?;
    // `<base>/latest` is what a channel without an explicit --version reads.
    std::fs::write(out_dir.join("latest"), format!("{tag}\n"))?;

    let pubkey = public_b64(&key);
    println!("channel ready at {}", out_dir.display());
    println!("  {tag}/{artifact}");
    println!("  {tag}/{artifact}.minisig");
    println!();
    println!("Serve it:");
    println!(
        "  python3 -m http.server 8000 --directory {}",
        out_dir.display()
    );
    println!();
    println!("Point every node at it — in `meshnode.toml`, then restart or SIGHUP:");
    println!();
    println!("  [release]");
    println!("  base = \"http://<this-host>:8000\"");
    println!("  pubkey = \"{pubkey}\"");
    println!();
    println!("Then, from an admin:");
    println!("  meshnode upgrade --status --to {bare}");
    println!("  meshnode upgrade --on tag=<ring> --to {bare}");
    println!();
    println!(
        "NOTE: this is a dev key living beside the artifacts it signs. Anyone who can write to \
         that directory can hand your fleet a binary. Never point a real mesh at one."
    );
    Ok(())
}

/// One dev key per channel directory, generated on first use.
fn load_or_generate_key(out_dir: &Path) -> Result<SigningKey> {
    let path = out_dir.join("dev-signing.key");
    if path.exists() {
        let seed: [u8; 32] = std::fs::read(&path)?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is not a 32-byte seed", path.display()))?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    std::fs::create_dir_all(out_dir)?;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("system rng: {e}"))?;
    std::fs::write(&path, seed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("generated a new dev signing key at {}", path.display());
    Ok(SigningKey::from_bytes(&seed))
}

fn public_b64(key: &SigningKey) -> String {
    let mut bin = Vec::with_capacity(42);
    bin.extend_from_slice(b"Ed");
    bin.extend_from_slice(&DEV_KEY_ID);
    bin.extend_from_slice(key.verifying_key().as_bytes());
    data_encoding::BASE64.encode(&bin)
}

/// A legacy (`Ed`) minisign signature: Ed25519 over the file itself.
///
/// `meshnode` verifies with `allow_legacy = true`, so this is accepted
/// exactly as a prehashed one is, and it needs no BLAKE2b of its own.
fn sign_legacy(key: &SigningKey, file: &[u8], trusted: &str) -> String {
    let sig = key.sign(file).to_bytes();
    let mut bin = Vec::with_capacity(74);
    bin.extend_from_slice(b"Ed");
    bin.extend_from_slice(&DEV_KEY_ID);
    bin.extend_from_slice(&sig);
    let mut global = Vec::with_capacity(64 + trusted.len());
    global.extend_from_slice(&sig);
    global.extend_from_slice(trusted.as_bytes());
    let gsig = key.sign(&global).to_bytes();
    format!(
        "untrusted comment: meshware dev release channel\n{}\ntrusted comment: {trusted}\n{}\n",
        data_encoding::BASE64.encode(&bin),
        data_encoding::BASE64.encode(&gsig)
    )
}
