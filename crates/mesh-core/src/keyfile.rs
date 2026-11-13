//! A user identity at rest: sealed, and unsealed once per login session (B9).
//!
//! ## The finding this closes
//!
//! A user key is 32 bytes and it is the whole identity. `identity.rs` said so
//! plainly for a long time — *"passphrase encryption of this file is a planned
//! refinement; today it is a mode-0600 seed like the node key"* — and for a
//! **node** key on a server that is a defensible trade, which `SECURITY.md`
//! makes explicitly: local root on a node is game over for that node.
//!
//! A human's key on a laptop is a different object. `SECURITY.md` also states
//! that a mesh admin can grant themselves anything, so those 32 bytes are the
//! most valuable secret a person holds — and mode 0600 stops exactly one
//! attacker, the other account on the same machine. It does not stop the three
//! that actually happen:
//!
//! - a backup, which copies the file verbatim to somewhere with different
//!   permissions and a longer retention than anybody thinks;
//! - a sync client — Dropbox, iCloud, `rsync` to a NAS — which is a backup
//!   nobody remembers configuring;
//! - a stolen or discarded laptop, where the disk outlives the login session.
//!
//! ## What this does, and what it deliberately does not
//!
//! The seed is sealed with XChaCha20-Poly1305 under a key-encryption key
//! derived from a passphrase with Argon2id. The sealed file replaces the
//! plaintext one. All three attacks above now get 105 bytes of ciphertext.
//!
//! **It does not stop a process running as you on an unlocked machine.**
//! Once unlocked, the key-encryption key sits in `$XDG_RUNTIME_DIR` for the
//! rest of the session and anything running as that user can read it — the
//! same account that could have read the plaintext key. This is
//! *presence-gating*, not hardware binding, and `docs/operating/limits.md`
//! says so in those words. Hardware binding is a token that never exports the
//! private key; it is a different feature and it is not this one.
//!
//! Saying that out loud is the point. A security control whose limits are
//! discovered by the reader is worth less than one whose limits are written
//! next to it.
//!
//! ## Why a session cache rather than an agent
//!
//! `Keypair::sign_request` signs **every** IPC call — `status`, `nodes`,
//! `jobs`, a shell completion. A design that asks for the passphrase on each
//! one is uninstalled the same afternoon, so something has to hold the unlock.
//!
//! The obvious answer is a long-lived agent process, and it was not taken. An
//! agent is a second daemon to supervise, a second socket to authorize, and a
//! lifetime nobody agrees on. `$XDG_RUNTIME_DIR` already provides exactly the
//! properties wanted, maintained by the operating system: it is tmpfs, so the
//! cache never reaches a disk or a backup; it is mode 0700 and per-user; and
//! `systemd-logind` removes the whole tree at logout. The cache is the
//! *key-encryption key*, not the seed, with an expiry inside it, so a stale
//! file is refused on read rather than trusted because it exists.
//!
//! Where there is no `$XDG_RUNTIME_DIR` — a cron job, a container, a
//! non-systemd box — there is no cache at all, and the caller is told to
//! export `MESHWARE_PASSPHRASE` or keep the key plaintext. Falling back to the
//! home directory would put the unlock on the disk this module exists to get
//! it off.
//!
//! ## The file
//!
//! ```text
//!   off  len  field
//!     0    6  magic "MWKEY1"
//!     6    1  format version (1)
//!     7    1  kdf id (1 = Argon2id)
//!     8    4  Argon2 memory cost, KiB, little-endian u32
//!    12    4  Argon2 time cost (passes), little-endian u32
//!    16    1  Argon2 lanes
//!    17   16  salt
//!    33   24  XChaCha20-Poly1305 nonce
//!    57   48  ciphertext (32-byte seed) ‖ Poly1305 tag
//!         = 105 bytes
//! ```
//!
//! Bytes `0..33` — everything through the salt — are passed as the AEAD's
//! associated data. Without that, an attacker who can write the file could
//! rewrite the memory cost to 8 KiB and hand the owner's own tooling a
//! cheap-to-crack version of their key; the tag covers the parameters, so a
//! tampered header fails to open rather than opening weakly.
//!
//! A legacy plaintext key is exactly 32 bytes and has no magic, which is how
//! [`Stored::read`] tells them apart. That is a real distinction and not a
//! guess: an Ed25519 seed cannot begin with `MWKEY1` and also be 32 bytes long
//! with the rest of a valid header behind it.

use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroize;

use crate::error::{CoreError, Result};

const MAGIC: &[u8; 6] = b"MWKEY1";
/// A key file that points at hardware instead of holding a key (Y2/Y3).
///
/// A distinct magic rather than a flag inside the sealed header, so the
/// three forms are told apart by the first six bytes and an older build
/// says "not a meshware identity" instead of trying to derive a key from a
/// public one.
const HW_MAGIC: &[u8; 6] = b"MWHWK1";
const VERSION: u8 = 1;
const KDF_ARGON2ID: u8 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
/// 32-byte seed plus the 16-byte Poly1305 tag.
const CT_LEN: usize = 48;
const HEADER_LEN: usize = 33;
const SEALED_LEN: usize = HEADER_LEN + NONCE_LEN + CT_LEN;

/// A plaintext seed is exactly this long, which is what makes the two
/// formats distinguishable without a flag file.
const SEED_LEN: usize = 32;
/// `MWHWK1` + version + the 32-byte public key.
const HW_LEN: usize = 6 + 1 + SEED_LEN;

/// Argon2id cost. 64 MiB / 3 passes / 1 lane is the OWASP baseline and takes
/// roughly a tenth of a second on a laptop — which is the correct place to be
/// for something typed once per login session rather than per request.
///
/// Written into every file that is sealed, and read back from the file rather
/// than from here, so raising these later does not lock anybody out of a key
/// sealed under the old cost.
const M_COST_KIB: u32 = 65_536;
const T_COST: u32 = 3;
const LANES: u8 = 1;

/// How long an unlock lasts before the passphrase is asked for again.
///
/// Twelve hours covers a working day without covering a week. The runtime
/// directory usually outlives this — it survives until logout — so the expiry
/// is the shorter of the two bounds and the one that does the work.
const UNLOCK_TTL_SECS: u64 = 12 * 60 * 60;

/// Expiry stamp plus the key-encryption key.
const CACHE_LEN: usize = 8 + 32;

/// Environment variable holding a passphrase for non-interactive use.
///
/// For CI, a cron job, and a container — the places with no terminal to
/// prompt on. It is read once and zeroized; it is still an environment
/// variable, which means it is visible in `/proc/<pid>/environ` to the same
/// account, and that is documented rather than pretended away.
pub const PASSPHRASE_ENV: &str = "MESHWARE_PASSPHRASE";

/// What is actually in a user key file.
#[derive(Debug)]
pub enum Stored {
    /// A bare 32-byte seed, mode 0600. The format before B9 and still valid.
    Plain([u8; SEED_LEN]),
    /// Sealed under a passphrase.
    Sealed(Sealed),
    /// **No private key at all** — a reference to one an agent holds on
    /// hardware (Y2/Y3). The 32 bytes are the *public* key, which is also
    /// the `UserId`.
    ///
    /// A third form of the same file rather than a marker beside it,
    /// because the question "where is this person's key" has to have one
    /// answer in one place. Two files would mean a state in which they
    /// disagree, and the way that goes wrong is a stale marker silently
    /// pointing at a software key somebody thought they had retired.
    ///
    /// Nothing here is secret, so the file's 0600 mode is habit rather than
    /// protection — and the fact that it is *not* a secret is the point.
    /// A backup of this directory, a sync client, or another process
    /// running as the same account now gets a public key and a note about
    /// where the private half is not.
    Hardware([u8; SEED_LEN]),
}

/// A sealed key file, parsed but not opened.
#[derive(Debug, Clone)]
pub struct Sealed {
    m_cost: u32,
    t_cost: u32,
    lanes: u8,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    ct: [u8; CT_LEN],
    /// Bytes `0..33` verbatim, kept so the AEAD sees exactly what was on
    /// disk rather than a re-serialization that could differ by a byte.
    aad: [u8; HEADER_LEN],
}

impl Stored {
    /// Read and classify a key file.
    pub fn read(path: &Path) -> Result<Stored> {
        // A plaintext key file *is* the seed (SEC-23).
        let bytes = zeroize::Zeroizing::new(std::fs::read(path)?);
        Stored::parse(&bytes).map_err(|e| CoreError::Identity(format!("{}: {e}", path.display())))
    }

    /// Classify bytes that are not (yet) a file.
    ///
    /// `protect` uses this to prove a freshly sealed key reopens *before* it
    /// overwrites the plaintext one — the one failure that command must not
    /// be able to produce.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Stored> {
        Stored::parse(bytes).map_err(CoreError::Identity)
    }

    fn parse(bytes: &[u8]) -> std::result::Result<Stored, String> {
        if bytes.len() == SEED_LEN && !bytes.starts_with(MAGIC) {
            let mut seed = [0u8; SEED_LEN];
            seed.copy_from_slice(bytes);
            return Ok(Stored::Plain(seed));
        }
        if bytes.starts_with(HW_MAGIC) {
            if bytes.len() != HW_LEN {
                return Err(format!(
                    "hardware identity is {} bytes, expected {HW_LEN}",
                    bytes.len()
                ));
            }
            let version = bytes[6];
            if version != VERSION {
                return Err(format!(
                    "hardware identity is format version {version}; this build understands \
                     {VERSION}. Upgrade meshnode."
                ));
            }
            let mut public = [0u8; SEED_LEN];
            public.copy_from_slice(&bytes[7..HW_LEN]);
            return Ok(Stored::Hardware(public));
        }
        if !bytes.starts_with(MAGIC) {
            return Err(format!(
                "not a meshware identity: expected {SEED_LEN} raw bytes or a sealed key, \
                 found {} bytes",
                bytes.len()
            ));
        }
        if bytes.len() != SEALED_LEN {
            return Err(format!(
                "sealed identity is {} bytes, expected {SEALED_LEN}",
                bytes.len()
            ));
        }
        let version = bytes[6];
        if version != VERSION {
            return Err(format!(
                "sealed identity is format version {version}; this build understands {VERSION}. \
                 Upgrade meshnode."
            ));
        }
        let kdf = bytes[7];
        if kdf != KDF_ARGON2ID {
            return Err(format!("sealed identity uses unknown kdf id {kdf}"));
        }
        let m_cost = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
        let t_cost = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes"));
        let lanes = bytes[16];
        // A file claiming zero passes or one KiB would derive a key in no
        // time at all, which is the downgrade the AAD exists to prevent —
        // but a floor here means a malformed file is refused before any
        // work is done rather than after.
        if m_cost < 8 * 1024 || t_cost == 0 || lanes == 0 {
            return Err(format!(
                "sealed identity declares an unsafe kdf cost (m={m_cost}KiB, t={t_cost}, \
                 lanes={lanes}); refusing to open it"
            ));
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[17..33]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[33..57]);
        let mut ct = [0u8; CT_LEN];
        ct.copy_from_slice(&bytes[57..105]);
        let mut aad = [0u8; HEADER_LEN];
        aad.copy_from_slice(&bytes[..HEADER_LEN]);
        Ok(Stored::Sealed(Sealed {
            m_cost,
            t_cost,
            lanes,
            salt,
            nonce,
            ct,
            aad,
        }))
    }

    /// Whether this file needs a passphrase.
    pub fn is_sealed(&self) -> bool {
        matches!(self, Stored::Sealed(_))
    }
}

/// Is the key at `path` sealed? `false` for a plaintext key, a missing file,
/// or anything unreadable — every one of which is a state some caller has to
/// handle anyway, and none of which is "locked".
pub fn is_sealed(path: &Path) -> bool {
    matches!(Stored::read(path), Ok(s) if s.is_sealed())
}

impl Sealed {
    /// Derive the key-encryption key from a passphrase, at this file's cost.
    ///
    /// The parameters come from the file and not from the constants above:
    /// a key sealed last year under a lower memory cost must still open on a
    /// build whose default has since gone up.
    pub fn kek(&self, passphrase: &[u8]) -> Result<[u8; 32]> {
        let params = Params::new(self.m_cost, self.t_cost, self.lanes as u32, Some(32))
            .map_err(|e| CoreError::Identity(format!("argon2 parameters: {e}")))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut kek = [0u8; 32];
        argon
            .hash_password_into(passphrase, &self.salt, &mut kek)
            .map_err(|e| CoreError::Identity(format!("argon2: {e}")))?;
        Ok(kek)
    }

    /// Unwrap the seed with an already-derived key-encryption key.
    pub fn open_with_kek(&self, kek: &[u8; 32]) -> Result<[u8; SEED_LEN]> {
        let aead = XChaCha20Poly1305::new(&Key::from(*kek));
        let plain = aead
            .decrypt(
                &XNonce::from(self.nonce),
                Payload {
                    msg: &self.ct,
                    aad: &self.aad,
                },
            )
            .map_err(|_| {
                CoreError::Identity("wrong passphrase, or this key file has been modified".into())
            })?;
        if plain.len() != SEED_LEN {
            return Err(CoreError::Identity(format!(
                "sealed identity unwrapped to {} bytes, expected {SEED_LEN}",
                plain.len()
            )));
        }
        let mut seed = [0u8; SEED_LEN];
        seed.copy_from_slice(&plain);
        Ok(seed)
    }

    /// Unwrap the seed with a passphrase.
    pub fn open(&self, passphrase: &[u8]) -> Result<[u8; SEED_LEN]> {
        let mut kek = self.kek(passphrase)?;
        let out = self.open_with_kek(&kek);
        kek.zeroize();
        out
    }
}

/// The bytes of a key file that points at hardware (Y2/Y3).
///
/// Takes the **public** key, because that is all there is: the private half
/// is on a card and this file is a note saying so.
pub fn hardware_ref(public: &[u8; SEED_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HW_LEN);
    out.extend_from_slice(HW_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(public);
    out
}

/// Seal a seed under a passphrase, returning the bytes to write.
pub fn seal(seed: &[u8; SEED_LEN], passphrase: &[u8]) -> Result<Vec<u8>> {
    if passphrase.is_empty() {
        return Err(CoreError::Identity(
            "an empty passphrase protects nothing; choose one or leave the key unsealed".into(),
        ));
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut salt)
        .map_err(|e| CoreError::Identity(format!("system rng unavailable: {e}")))?;
    getrandom::fill(&mut nonce)
        .map_err(|e| CoreError::Identity(format!("system rng unavailable: {e}")))?;

    let mut out = Vec::with_capacity(SEALED_LEN);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(KDF_ARGON2ID);
    out.extend_from_slice(&M_COST_KIB.to_le_bytes());
    out.extend_from_slice(&T_COST.to_le_bytes());
    out.push(LANES);
    out.extend_from_slice(&salt);
    debug_assert_eq!(out.len(), HEADER_LEN);

    let mut aad = [0u8; HEADER_LEN];
    aad.copy_from_slice(&out);

    // Derive through the same path a reader will take, so the two can never
    // disagree about what the header means.
    let sealed = Sealed {
        m_cost: M_COST_KIB,
        t_cost: T_COST,
        lanes: LANES,
        salt,
        nonce,
        ct: [0u8; CT_LEN],
        aad,
    };
    let mut kek = sealed.kek(passphrase)?;
    let aead = XChaCha20Poly1305::new(&Key::from(kek));
    let ct = aead
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: seed.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| CoreError::Identity("sealing the identity failed".into()))?;
    kek.zeroize();

    if ct.len() != CT_LEN {
        return Err(CoreError::Identity(format!(
            "sealed to {} bytes, expected {CT_LEN}",
            ct.len()
        )));
    }
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    debug_assert_eq!(out.len(), SEALED_LEN);
    Ok(out)
}

// ---------------------------------------------------------------------------
// The session cache
// ---------------------------------------------------------------------------

/// Where the unlock for `key_path` lives for the rest of this login session.
///
/// `None` when there is no `$XDG_RUNTIME_DIR`, and that is a refusal rather
/// than a gap: the whole value of the cache is that it lives on tmpfs the
/// system clears at logout. Putting it in `~/.meshware` instead would write
/// the unlock to the same disk, and the same backup, that this module exists
/// to get the key off.
pub fn cache_path(key_path: &Path) -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(cache_path_in(Path::new(&runtime), key_path))
}

/// [`cache_path`] with the runtime directory passed in.
///
/// Split out for the same reason `identity::load_cli_user_from` is: the
/// alternative is a test that writes to the process environment, and
/// `std::env::set_var` is `unsafe` under the 2024 edition — which this
/// workspace denies outright.
fn cache_path_in(runtime: &Path, key_path: &Path) -> PathBuf {
    // Keyed by the key file's own path, the way `socket_path_for` keys a
    // socket by its data dir: two identities on one machine — a personal key
    // and a founder key under a different home — must not share an unlock.
    let canon = std::fs::canonicalize(key_path).unwrap_or_else(|_| key_path.to_path_buf());
    let hash = blake3::hash(canon.as_os_str().as_encoded_bytes());
    let short = data_encoding::HEXLOWER.encode(&hash.as_bytes()[..8]);
    runtime.join("meshware").join(format!("unlock-{short}"))
}

/// Remember an unlock until [`UNLOCK_TTL_SECS`] from now.
pub fn cache_put(key_path: &Path, kek: &[u8; 32]) -> Result<()> {
    let Some(path) = cache_path(key_path) else {
        return Err(CoreError::Identity(
            "no $XDG_RUNTIME_DIR, so there is nowhere to keep an unlock that a backup \
             will not reach. Set MESHWARE_PASSPHRASE for this command instead."
                .into(),
        ));
    };
    cache_put_at(&path, kek)
}

fn cache_put_at(path: &Path, kek: &[u8; 32]) -> Result<()> {
    let mut buf = Vec::with_capacity(CACHE_LEN);
    buf.extend_from_slice(&(now_secs() + UNLOCK_TTL_SECS).to_le_bytes());
    buf.extend_from_slice(kek);
    let out = write_private(path, &buf);
    buf.zeroize();
    out
}

/// The remembered unlock for `key_path`, if there is one and it is live.
///
/// An expired or malformed cache is removed rather than reported: the caller
/// is about to ask for a passphrase either way, and leaving a dead file to be
/// re-read on every command is how a cache becomes a bug.
pub fn cache_get(key_path: &Path) -> Option<[u8; 32]> {
    cache_get_at(&cache_path(key_path)?)
}

fn cache_get_at(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != CACHE_LEN {
        let _ = std::fs::remove_file(path);
        return None;
    }
    let expiry = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    if expiry <= now_secs() {
        let _ = std::fs::remove_file(path);
        return None;
    }
    let mut kek = [0u8; 32];
    kek.copy_from_slice(&bytes[8..]);
    Some(kek)
}

/// Forget the unlock for `key_path`. Idempotent — locking an already-locked
/// identity is a no-op and not an error.
pub fn cache_clear(key_path: &Path) {
    if let Some(path) = cache_path(key_path) {
        let _ = std::fs::remove_file(path);
    }
}

/// Seconds remaining on the cached unlock, if any. For `identity status`,
/// which owes a person a straight answer about whether they are about to be
/// asked for a passphrase.
pub fn cache_remaining(key_path: &Path) -> Option<u64> {
    let bytes = std::fs::read(cache_path(key_path)?).ok()?;
    if bytes.len() != CACHE_LEN {
        return None;
    }
    let expiry = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    expiry.checked_sub(now_secs()).filter(|s| *s > 0)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write mode 0600, created that way rather than fixed up afterwards.
///
/// The same rule as `Keypair::persist`, and for the same reason: create-then-
/// chmod leaves a window at `0666 & ~umask` in which a secret is world-
/// readable. Small, and real on a shared machine.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_key_round_trips() {
        let seed = [7u8; 32];
        let bytes = seal(&seed, b"correct horse battery staple").expect("seal");
        assert_eq!(bytes.len(), SEALED_LEN);
        let Stored::Sealed(s) = Stored::parse(&bytes).expect("parse") else {
            panic!("expected a sealed key");
        };
        assert_eq!(s.open(b"correct horse battery staple").expect("open"), seed);
    }

    #[test]
    fn the_wrong_passphrase_is_refused_and_says_so() {
        let bytes = seal(&[3u8; 32], b"right").expect("seal");
        let Stored::Sealed(s) = Stored::parse(&bytes).expect("parse") else {
            panic!("expected a sealed key");
        };
        let err = s.open(b"wrong").expect_err("must not open").to_string();
        assert!(err.contains("wrong passphrase"), "unhelpful error: {err}");
    }

    /// The AAD is the whole reason the cost parameters are safe to read from
    /// the file. Without it, anybody who can write the file can ask the
    /// owner's own binary to re-derive their key at 8 KiB of memory.
    /// Downgrading the declared memory cost must not yield a key that opens
    /// under a cheaper derivation. 16 MiB is above the parse-time floor, so
    /// this reaches the AEAD and is caught by the tag rather than by the
    /// bounds check — which is the property actually under test.
    #[test]
    fn a_tampered_header_will_not_open() {
        let mut bytes = seal(&[9u8; 32], b"pass").expect("seal");
        bytes[8..12].copy_from_slice(&(16u32 * 1024).to_le_bytes());
        let Stored::Sealed(s) = Stored::parse(&bytes).expect("parse") else {
            panic!("expected a sealed key");
        };
        assert!(s.open(b"pass").is_err(), "a downgraded cost must not open");

        // ...and the same for the salt, which is also inside the AAD.
        let mut bytes = seal(&[9u8; 32], b"pass").expect("seal");
        bytes[20] ^= 0xff;
        let Stored::Sealed(s) = Stored::parse(&bytes).expect("parse") else {
            panic!("expected a sealed key");
        };
        assert!(s.open(b"pass").is_err(), "a tampered salt must not open");
    }

    /// A plaintext key from before B9 has to keep working, and a sealed one
    /// has to be recognised without a flag file beside it.
    #[test]
    fn the_two_formats_are_told_apart_by_their_bytes() {
        let plain = [1u8; 32];
        assert!(matches!(
            Stored::parse(&plain).expect("parse"),
            Stored::Plain(_)
        ));

        let sealed = seal(&[1u8; 32], b"p").expect("seal");
        assert!(Stored::parse(&sealed).expect("parse").is_sealed());

        // Neither shape: a truncated file must be an error, never a key.
        assert!(Stored::parse(&[0u8; 17]).is_err());
        assert!(Stored::parse(b"MWKEY1 short").is_err());
    }

    #[test]
    fn an_empty_passphrase_is_refused() {
        let err = seal(&[0u8; 32], b"").expect_err("must refuse").to_string();
        assert!(err.contains("empty passphrase"), "{err}");
    }

    /// A cost floor means a malformed or hostile file is refused during
    /// parsing rather than after a derivation nobody wanted to run.
    #[test]
    fn an_absurd_declared_cost_is_refused_at_parse_time() {
        let mut bytes = seal(&[2u8; 32], b"p").expect("seal");
        bytes[12..16].copy_from_slice(&0u32.to_le_bytes()); // zero passes
        let err = Stored::parse(&bytes).expect_err("must refuse").to_string();
        assert!(err.contains("unsafe kdf cost"), "{err}");
    }

    #[test]
    fn the_session_cache_expires_and_is_scoped_to_one_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = dir.path().join("run");
        let a = dir.path().join("a.key");
        let b = dir.path().join("b.key");
        std::fs::write(&a, [0u8; 32]).expect("write a");
        std::fs::write(&b, [0u8; 32]).expect("write b");
        let (pa, pb) = (cache_path_in(&runtime, &a), cache_path_in(&runtime, &b));

        // Two identities on one machine must not share an unlock.
        assert_ne!(pa, pb, "each key file gets its own unlock");

        assert!(
            cache_get_at(&pa).is_none(),
            "nothing is unlocked to start with"
        );
        cache_put_at(&pa, &[42u8; 32]).expect("put");
        assert_eq!(cache_get_at(&pa), Some([42u8; 32]));
        assert!(cache_get_at(&pb).is_none(), "unlocking a did not unlock b");

        // An expired unlock is refused *and* removed, so a dead file is not
        // re-read on every subsequent command.
        let mut stale = Vec::new();
        stale.extend_from_slice(&(now_secs() - 1).to_le_bytes());
        stale.extend_from_slice(&[42u8; 32]);
        write_private(&pa, &stale).expect("write stale");
        assert!(
            cache_get_at(&pa).is_none(),
            "an expired unlock must be refused"
        );
        assert!(!pa.exists(), "an expired unlock must be removed");

        // So is a truncated one — a half-written cache is not a key.
        write_private(&pa, b"short").expect("write short");
        assert!(
            cache_get_at(&pa).is_none(),
            "a malformed unlock must be refused"
        );
        assert!(!pa.exists(), "a malformed unlock must be removed");
    }

    /// The unlock must never be written anywhere a backup would find it.
    #[test]
    fn the_cache_lives_only_under_the_runtime_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("user.key");
        std::fs::write(&key, [0u8; 32]).expect("write");
        let runtime = Path::new("/run/user/1000");
        assert!(
            cache_path_in(runtime, &key).starts_with(runtime),
            "the unlock must stay inside $XDG_RUNTIME_DIR"
        );
    }

    #[cfg(unix)]
    #[test]
    fn everything_written_here_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("secret");
        write_private(&path, b"x").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "found mode {:o}", mode & 0o777);
    }
}
