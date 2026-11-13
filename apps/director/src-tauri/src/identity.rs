//! The key-at-rest dialog: Director's half of B9.
//!
//! `meshnode identity protect | unlock | lock` gave the CLI a way to seal a
//! user key and open it once per login session. Director read the same file
//! and the same session unlock — but had no way to *ask*. With a sealed key
//! and no unlock, every action failed with the locked message and a GUI-only
//! person had nowhere to go but a terminal. That is rule D-E3's whole point:
//! a capability that lands only in the CLI is a capability half the product
//! does not have.
//!
//! ## One state, two doors
//!
//! Nothing here is a second implementation. The format, the KDF and the
//! session cache all live in [`mesh_core::keyfile`]; these commands are the
//! window's way of reaching them. So `meshnode identity unlock` in a
//! terminal unlocks Director, and this dialog unlocks the CLI, because both
//! read `$XDG_RUNTIME_DIR` and there is only one unlock to find.
//!
//! ## Why every command runs on the blocking pool
//!
//! Argon2id at 64 MiB is *deliberately* slow — around a tenth of a second,
//! which is the point of a memory-hard KDF and far too long to hold an async
//! worker. The same reasoning `credentials.rs` gives for the keyring calls
//! beside it: work that blocks belongs on the blocking pool, not on a thread
//! the UI is waiting on.
//!
//! ## The passphrase never becomes a `String` that outlives the call
//!
//! It arrives as one — it came through a JSON command boundary and there is
//! no way around that — and is moved into [`Zeroizing`] immediately, so the
//! buffer is wiped when the command returns rather than left in the heap for
//! whatever reads it next.

use zeroize::Zeroizing;

use mesh_core::identity::{Keypair, default_user_identity_path};
use mesh_core::keyfile::{self, Stored};

use crate::view::IdentityLockView;

/// The key these commands act on: this person's own, the same file the CLI
/// resolves with no `--data-dir`.
///
/// Deliberately not the engine's `<home>/director/identity/user.key`. That
/// one is a mirror written at founding so genesis can name the founder; it
/// is machine-held from then on, and sealing it would be sealing the wrong
/// thing — see `engine::mirror_user_for_bootstrap`.
fn key_path() -> std::path::PathBuf {
    default_user_identity_path()
}

/// Read the lock state. Cheap, and safe to call on every boot.
#[tauri::command]
pub async fn identity_lock_status() -> Result<IdentityLockView, String> {
    tauri::async_runtime::spawn_blocking(status_now)
        .await
        .map_err(|e| e.to_string())?
}

fn status_now() -> Result<IdentityLockView, String> {
    let path = key_path();
    let exists = path.exists();
    let protected = exists && keyfile::is_sealed(&path);
    let bound = match Stored::read(&path) {
        Ok(Stored::Hardware(public)) if exists => Some(public),
        _ => None,
    };
    let hardware = bound.is_some();
    let hardware_ready = bound.is_some_and(|public| {
        mesh_core::hardware::agent_socket(None)
            .and_then(|socket| mesh_core::hardware::find(&socket, &public))
            .is_ok()
    });
    let remaining = keyfile::cache_remaining(&path);
    Ok(IdentityLockView {
        path: path.display().to_string(),
        exists,
        protected,
        hardware,
        hardware_ready,
        unlocked: remaining.is_some(),
        unlocked_for_secs: remaining.map(|s| s.to_string()),
        session_store: keyfile::cache_path(&path).map(|p| p.display().to_string()),
    })
}

/// Open the key for this login session.
#[tauri::command]
pub async fn identity_unlock(passphrase: String) -> Result<IdentityLockView, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pass = Zeroizing::new(passphrase);
        let path = key_path();
        Keypair::unlock_and_remember(&path, pass.as_bytes()).map_err(|e| e.to_string())?;
        status_now()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forget the unlock now.
///
/// Not a destructive act and not confirmed: the key is untouched, and the
/// worst case is typing a passphrase again.
#[tauri::command]
pub async fn identity_lock_now() -> Result<IdentityLockView, String> {
    tauri::async_runtime::spawn_blocking(|| {
        keyfile::cache_clear(&key_path());
        status_now()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Seal a plaintext key under a passphrase.
///
/// Verifies the sealed bytes reopen to the same identity *before* the
/// plaintext is overwritten — the same order `meshnode identity protect`
/// uses, and for the same reason: producing a file that does not decrypt is
/// the one failure this must not be able to cause, because there is no copy
/// of the key to go back to.
#[tauri::command]
pub async fn identity_protect(passphrase: String) -> Result<IdentityLockView, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pass = Zeroizing::new(passphrase);
        if pass.is_empty() {
            return Err("choose a passphrase — an empty one protects nothing".to_string());
        }
        let path = key_path();
        let seed = match Stored::read(&path).map_err(|e| e.to_string())? {
            Stored::Plain(seed) => seed,
            Stored::Sealed(_) => return Err("this key is already protected".to_string()),
            Stored::Hardware(_) => {
                return Err(
                    "this key is on a security key — there is nothing on this disk to \
                            protect"
                        .to_string(),
                );
            }
        };

        let sealed = keyfile::seal(&seed, pass.as_bytes()).map_err(|e| e.to_string())?;
        let Stored::Sealed(check) = Stored::parse_bytes(&sealed).map_err(|e| e.to_string())? else {
            return Err("internal error: a freshly sealed key did not parse as sealed".into());
        };
        let reopened = check.open(pass.as_bytes()).map_err(|e| e.to_string())?;
        if Keypair::from_seed(reopened).user_id() != Keypair::from_seed(seed).user_id() {
            return Err(
                "internal error: the sealed key did not reopen to the same identity".into(),
            );
        }

        keyfile::write_private(&path, &sealed).map_err(|e| e.to_string())?;
        // Unlock it straight away. Protecting a key and then being asked for
        // the passphrase you just chose is a pointless second step, and the
        // person is demonstrably present.
        Keypair::unlock_and_remember(&path, pass.as_bytes()).map_err(|e| e.to_string())?;
        status_now()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Put the key back to a plaintext seed.
///
/// Present because a one-way door is a trap. It needs the passphrase, so it
/// is not something a passer-by at an unlocked screen can do.
#[tauri::command]
pub async fn identity_unprotect(passphrase: String) -> Result<IdentityLockView, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pass = Zeroizing::new(passphrase);
        let path = key_path();
        let Stored::Sealed(sealed) = Stored::read(&path).map_err(|e| e.to_string())? else {
            return Err("this key is not protected".to_string());
        };
        let seed = sealed.open(pass.as_bytes()).map_err(|e| e.to_string())?;
        Keypair::from_seed(seed)
            .persist(&path)
            .map_err(|e| e.to_string())?;
        keyfile::cache_clear(&path);
        status_now()
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view a locked key produces is what the gate keys off, so the two
    /// booleans have to mean what the gate thinks they mean: a sealed key
    /// with no session is `protected && !unlocked`, and that pair is the
    /// only state that should stop Director.
    #[test]
    fn a_sealed_key_with_no_session_reads_as_protected_and_locked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        let kp = Keypair::generate().expect("generate");
        let sealed = keyfile::seal(&kp.seed_bytes().expect("software key"), b"pw").expect("seal");
        keyfile::write_private(&path, &sealed).expect("write");

        assert!(keyfile::is_sealed(&path), "the file is sealed");
        // No unlock has been made for this path, so there is no session.
        assert!(
            keyfile::cache_remaining(&path).is_none(),
            "a key nobody unlocked must not read as unlocked"
        );
    }

    #[test]
    fn a_plaintext_key_is_not_protected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        Keypair::generate()
            .expect("generate")
            .persist(&path)
            .expect("persist");
        assert!(!keyfile::is_sealed(&path));
    }
}
