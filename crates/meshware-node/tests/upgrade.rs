//! `meshnode upgrade --self` (MASTERPLAN M7) against a local HTTP fixture
//! and a test-only minisign signer: the happy path swaps the binary
//! atomically after verifying the signature and running `--version`; bad
//! signatures, the placeholder key, and `--check` never touch the binary.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer, SigningKey};
use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::identity::Keypair;
use meshware_node::upgrade::{self, DaemonOutcome, UpgradeOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// BLAKE2b-512 (minisign prehashes files before signing). ~60 lines, checked
// against the RFC 7693 "abc" vector below so a bug here fails loudly.
// ---------------------------------------------------------------------------

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];
const SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

fn g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

fn compress(h: &mut [u64; 8], m: &[u64; 16], t: u128, last: bool) {
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    for s in SIGMA.iter() {
        g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

fn blake2b512(data: &[u8]) -> [u8; 64] {
    let mut h = IV;
    h[0] ^= 0x0101_0000 ^ 64;
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(128).collect()
    };
    let n = blocks.len();
    let mut t: u128 = 0;
    for (i, block) in blocks.iter().enumerate() {
        t += block.len() as u128;
        let mut padded = [0u8; 128];
        padded[..block.len()].copy_from_slice(block);
        let mut m = [0u64; 16];
        for (j, word) in m.iter_mut().enumerate() {
            *word = u64::from_le_bytes(padded[j * 8..j * 8 + 8].try_into().unwrap());
        }
        compress(&mut h, &m, t, i == n - 1);
    }
    let mut out = [0u8; 64];
    for (i, word) in h.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    out
}

#[test]
fn blake2b_vector() {
    let hex = data_encoding::HEXLOWER.encode(&blake2b512(b"abc"));
    assert_eq!(
        hex,
        "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
    );
}

// ---------------------------------------------------------------------------
// Test-only minisign signer (prehashed "ED" mode, like `minisign -HSm`).
// ---------------------------------------------------------------------------

struct Signer2 {
    key: SigningKey,
    key_id: [u8; 8],
}

impl Signer2 {
    fn new(seed: u8) -> Self {
        Signer2 {
            key: SigningKey::from_bytes(&[seed; 32]),
            key_id: [seed, 2, 3, 4, 5, 6, 7, 8],
        }
    }

    fn public_b64(&self) -> String {
        let mut bin = Vec::with_capacity(42);
        bin.extend_from_slice(b"Ed");
        bin.extend_from_slice(&self.key_id);
        bin.extend_from_slice(self.key.verifying_key().as_bytes());
        data_encoding::BASE64.encode(&bin)
    }

    fn sign(&self, file: &[u8], trusted: &str) -> String {
        let digest = blake2b512(file);
        let sig = self.key.sign(&digest).to_bytes();
        let mut bin1 = Vec::with_capacity(74);
        bin1.extend_from_slice(b"ED");
        bin1.extend_from_slice(&self.key_id);
        bin1.extend_from_slice(&sig);
        let mut global = Vec::with_capacity(64 + trusted.len());
        global.extend_from_slice(&sig);
        global.extend_from_slice(trusted.as_bytes());
        let gsig = self.key.sign(&global).to_bytes();
        format!(
            "untrusted comment: signature from meshware test key\n{}\ntrusted comment: {trusted}\n{}\n",
            data_encoding::BASE64.encode(&bin1),
            data_encoding::BASE64.encode(&gsig)
        )
    }
}

// ---------------------------------------------------------------------------
// HTTP fixture: a static file server with a path → body map; `.minisig`
// bodies are served chunked to exercise dechunking, and `/redir/…` 302s to
// the real path (GitHub's download redirect).
// ---------------------------------------------------------------------------

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

async fn serve(files: Files) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let files = files.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let response: Vec<u8> = if let Some(real) = path.strip_prefix("/redir") {
                    format!("HTTP/1.1 302 Found\r\nLocation: {real}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .into_bytes()
                } else {
                    let body = files.lock().unwrap().get(&path).cloned();
                    match body {
                        Some(body) if path.ends_with(".minisig") => {
                            let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
                            for piece in body.chunks(7) {
                                out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                                out.extend_from_slice(piece);
                                out.extend_from_slice(b"\r\n");
                            }
                            out.extend_from_slice(b"0\r\n\r\n");
                            out
                        }
                        Some(body) => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(&body);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    }
                };
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// The "new release": a shell script that answers `--version`.
fn fake_binary(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho \"meshnode {version}\"\n").into_bytes()
}

fn publish(files: &Files, tag: &str, bin: &[u8], sig: &str) {
    let name = upgrade::artifact_name(tag);
    let mut f = files.lock().unwrap();
    f.insert("/latest".into(), tag.as_bytes().to_vec());
    f.insert(format!("/{tag}/{name}"), bin.to_vec());
    f.insert(format!("/{tag}/{name}.minisig"), sig.as_bytes().to_vec());
}

fn test_config(dir: &Path) -> NodeConfig {
    NodeConfig::load(
        None,
        &Overrides {
            data_dir: Some(dir.join("data")),
            listen: Some("127.0.0.1:0".into()),
            ..Default::default()
        },
    )
    .unwrap()
}

fn install_exe(dir: &Path) -> std::path::PathBuf {
    let exe = dir.join("bin").join("meshnode");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, fake_binary("0.0.0-old")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    exe
}

fn opts(exe: &Path, base: &str, pubkey: Option<&str>) -> UpgradeOptions {
    UpgradeOptions {
        exe: Some(exe.to_path_buf()),
        yes: true,
        release_base: Some(base.to_string()),
        pubkeys: pubkey.map(str::to_string).into_iter().collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn upgrade_installs_verified_binary_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let signer = Signer2::new(7);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    let bin = fake_binary("9.9.9");
    publish(
        &files,
        "v9.9.9",
        &bin,
        &signer.sign(&bin, "timestamp:1\tfile:meshnode"),
    );
    let base = serve(files.clone()).await;
    let key = signer.public_b64();
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();

    // --check: reports, touches nothing.
    let before = std::fs::read(&exe).unwrap();
    let report = upgrade::run(
        &config,
        &user,
        UpgradeOptions {
            check_only: true,
            ..opts(&exe, &base, Some(&key))
        },
    )
    .await
    .unwrap();
    assert_eq!(report.target, "9.9.9");
    assert!(!report.installed);
    assert_eq!(std::fs::read(&exe).unwrap(), before);

    // Real upgrade (through the GitHub-style redirect too).
    {
        let mut f = files.lock().unwrap();
        let name = upgrade::artifact_name("v9.9.9");
        let real_bin = f.get(&format!("/v9.9.9/{name}")).cloned().unwrap();
        let real_sig = f.get(&format!("/v9.9.9/{name}.minisig")).cloned().unwrap();
        // Serve the artifact only behind /redir so the client must follow.
        f.remove(&format!("/v9.9.9/{name}"));
        f.insert(format!("/real/v9.9.9/{name}"), real_bin);
        f.remove(&format!("/v9.9.9/{name}.minisig"));
        f.insert(format!("/real/v9.9.9/{name}.minisig"), real_sig);
        f.insert("/real/latest".into(), b"v9.9.9".to_vec());
    }
    let report = upgrade::run(
        &config,
        &user,
        opts(&exe, &format!("{base}/redir/real"), Some(&key)),
    )
    .await
    .unwrap();
    assert!(report.installed);
    assert_eq!(report.daemon, DaemonOutcome::NotRunning);
    assert_eq!(std::fs::read(&exe).unwrap(), bin);
    // No staging leftovers.
    let leftovers: Vec<_> = std::fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("upgrade-"))
        .collect();
    assert!(leftovers.is_empty(), "staging file must be renamed away");
    let out = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("9.9.9"));

    // The binary it replaced is kept, and the new one is on probation
    // (ENG-12).
    let prev = mesh_core::upgrade_guard::prev_path(&exe);
    assert_eq!(std::fs::read(&prev).unwrap(), before);
    let pending = mesh_core::upgrade_guard::pending(&config.data_dir).expect("no pending marker");
    assert_eq!(pending.to, "9.9.9");
    assert_eq!(pending.from, env!("CARGO_PKG_VERSION"));
    assert_eq!(pending.starts, 0);
    assert_eq!(pending.prev, prev);
}

/// `upgrade --rollback` puts the previous binary back, keeps the one it
/// replaced, and can only do it once per upgrade (ENG-12).
#[tokio::test]
async fn rollback_restores_the_previous_binary() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let old = std::fs::read(&exe).unwrap();
    let signer = Signer2::new(9);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    let bin = fake_binary("9.9.9");
    publish(
        &files,
        "v9.9.9",
        &bin,
        &signer.sign(&bin, "timestamp:1\tfile:meshnode"),
    );
    let base = serve(files.clone()).await;
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();

    // Nothing to roll back to yet.
    let refused = upgrade::rollback(&config, &user, opts(&exe, &base, None))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("no previous binary"),
        "{refused}"
    );

    upgrade::run(
        &config,
        &user,
        opts(&exe, &base, Some(&signer.public_b64())),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), bin);

    let report = upgrade::rollback(&config, &user, opts(&exe, &base, None))
        .await
        .unwrap();
    assert_eq!(report.target, "0.0.0-old");
    assert!(report.installed);
    assert_eq!(report.daemon, DaemonOutcome::NotRunning);
    assert_eq!(std::fs::read(&exe).unwrap(), old);
    assert_eq!(
        std::fs::read(mesh_core::upgrade_guard::failed_path(&exe)).unwrap(),
        bin
    );
    assert!(mesh_core::upgrade_guard::pending(&config.data_dir).is_none());
    assert!(
        upgrade::rollback(&config, &user, opts(&exe, &base, None))
            .await
            .is_err(),
        "a second rollback had something to restore"
    );
}

/// A previous binary that does not run is not put back.
#[tokio::test]
async fn rollback_refuses_a_previous_binary_that_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let prev = mesh_core::upgrade_guard::prev_path(&exe);
    std::fs::write(&prev, b"#!/bin/sh\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prev, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();
    let before = std::fs::read(&exe).unwrap();
    let refused = upgrade::rollback(&config, &user, opts(&exe, "http://127.0.0.1:9", None))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("does not run"), "{refused}");
    assert_eq!(std::fs::read(&exe).unwrap(), before);
    assert!(prev.exists());
}

#[tokio::test]
async fn bad_signature_is_refused_and_binary_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let signer = Signer2::new(7);
    let impostor = Signer2::new(9);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    let bin = fake_binary("9.9.9");
    // Signed by the wrong key.
    publish(
        &files,
        "v9.9.9",
        &bin,
        &impostor.sign(&bin, "timestamp:1\tfile:meshnode"),
    );
    let base = serve(files.clone()).await;
    let key = signer.public_b64();
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();
    let before = std::fs::read(&exe).unwrap();
    let err = upgrade::run(&config, &user, opts(&exe, &base, Some(&key)))
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("signature"),
        "{err:#}"
    );
    assert_eq!(std::fs::read(&exe).unwrap(), before);

    // Tampered body under a valid signature.
    let mut tampered = bin.clone();
    tampered.extend_from_slice(b"# evil\n");
    publish(
        &files,
        "v9.9.9",
        &tampered,
        &signer.sign(&bin, "timestamp:1\tfile:meshnode"),
    );
    let err = upgrade::run(&config, &user, opts(&exe, &base, Some(&key)))
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("signature"),
        "{err:#}"
    );
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[tokio::test]
async fn with_no_project_key_self_upgrade_refuses_and_says_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let signer = Signer2::new(7);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    let bin = fake_binary("9.9.9");
    publish(&files, "v9.9.9", &bin, &signer.sign(&bin, "t"));
    let base = serve(files.clone()).await;
    if std::env::var("MESHWARE_MINISIGN_PUBKEY").is_ok() {
        return; // the environment injects a key; the embedded path is not exercised
    }
    // No key given → the embedded packaging/minisign.pub, which carries no project key.
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();
    let before = std::fs::read(&exe).unwrap();
    let result = upgrade::run(&config, &user, opts(&exe, &base, None)).await;
    let embedded_is_placeholder = include_str!("../../../packaging/minisign.pub")
        .to_ascii_lowercase()
        .contains("placeholder");
    if embedded_is_placeholder {
        let err = result.unwrap_err();
        // It says there is no project key, and what to do instead.
        let msg = format!("{err:#}");
        assert!(msg.contains("no project-wide release key"), "{msg}");
        assert!(msg.contains("upgrade --publish"), "{msg}");
        assert_eq!(std::fs::read(&exe).unwrap(), before);
    } else {
        // A real key is pinned: our test signature must then be rejected.
        assert!(result.is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), before);
    }
}

#[tokio::test]
async fn sanity_check_failure_leaves_binary_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let signer = Signer2::new(7);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    // Validly signed, but claims the wrong version when run.
    let bin = fake_binary("1.2.3");
    publish(&files, "v9.9.9", &bin, &signer.sign(&bin, "t"));
    let base = serve(files.clone()).await;
    let key = signer.public_b64();
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();
    let before = std::fs::read(&exe).unwrap();
    let err = upgrade::run(&config, &user, opts(&exe, &base, Some(&key)))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("sanity"), "{err:#}");
    assert_eq!(std::fs::read(&exe).unwrap(), before);
    let leftovers: Vec<_> = std::fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("upgrade-"))
        .collect();
    assert!(leftovers.is_empty(), "staging file must be cleaned up");
}

#[tokio::test]
async fn already_up_to_date_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let exe = install_exe(dir.path());
    let signer = Signer2::new(7);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    let bin = fake_binary(env!("CARGO_PKG_VERSION"));
    publish(&files, &current, &bin, &signer.sign(&bin, "t"));
    let base = serve(files.clone()).await;
    let key = signer.public_b64();
    let config = test_config(dir.path());
    let user = Keypair::generate().unwrap();
    let before = std::fs::read(&exe).unwrap();
    let report = upgrade::run(&config, &user, opts(&exe, &base, Some(&key)))
        .await
        .unwrap();
    assert!(!report.installed);
    assert_eq!(report.daemon, DaemonOutcome::Untouched);
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[test]
fn verify_minisign_roundtrip() {
    let signer = Signer2::new(3);
    let data = b"release bytes";
    let sig = signer.sign(data, "timestamp:1\tfile:x");
    upgrade::verify_minisign(data, &sig, &signer.public_b64()).unwrap();
    assert!(upgrade::verify_minisign(b"other", &sig, &signer.public_b64()).is_err());
    assert!(upgrade::verify_minisign(data, &sig, &Signer2::new(4).public_b64()).is_err());
}
