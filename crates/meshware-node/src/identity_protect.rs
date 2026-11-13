//! `meshnode identity protect | unlock | lock | status` (B9).
//!
//! The four verbs that put a passphrase in front of a user key and then get
//! out of the way. The format and the crypto live in
//! [`mesh_core::keyfile`]; what is here is the part that talks to a person —
//! reading a passphrase without echoing it, and refusing to do the dangerous
//! things quietly.
//!
//! **None of these open an IPC connection.** Every other verb builds a `Ctx`,
//! which resolves and loads the user key — and a locked key makes that fail,
//! which would make `identity unlock` impossible to run for exactly the
//! person who needs it. So these resolve the key *path* and touch the file
//! directly. That is also why `unlock` works with the daemon stopped: the
//! unlock is a local fact about this login session, not something the mesh
//! has an opinion about.

use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use mesh_core::identity::{Keypair, cli_user_identity_path};
use mesh_core::keyfile::{self, Stored};
use zeroize::Zeroize;

/// Resolve which key file these verbs act on.
///
/// Deliberately *not* the same resolution `Ctx` does. `Ctx` will fall back to
/// this account's own key when the daemon's is unreadable, which is right for
/// signing a request and wrong here: "protect my key" must name one file and
/// act on that file, never quietly seal a different one.
pub fn target_path(data_dir: &Path, explicit: Option<&Path>) -> PathBuf {
    cli_user_identity_path(data_dir, explicit)
}

/// Read a passphrase from the terminal without echoing it.
///
/// `$MESHWARE_PASSPHRASE` wins when set, so CI and cron never reach the
/// terminal path at all. With no terminal and no variable this is an error
/// rather than a read from a pipe: a passphrase arriving on stdin would be
/// indistinguishable from a stray line of a script, and silently sealing a
/// key under the string `y` is not a failure anybody recovers from quickly.
fn read_passphrase(prompt: &str) -> Result<Vec<u8>> {
    if let Some(v) = std::env::var_os(keyfile::PASSPHRASE_ENV) {
        return Ok(v.as_encoded_bytes().to_vec());
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal to ask for a passphrase on. Set {} for this command.",
            keyfile::PASSPHRASE_ENV
        );
    }
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let _guard = NoEcho::enter();
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    println!();
    read.context("reading the passphrase")?;
    let out = line.trim_end_matches(['\n', '\r']).as_bytes().to_vec();
    line.zeroize();
    Ok(out)
}

/// Turn terminal echo off for as long as this value is alive.
///
/// A guard on the stack rather than two calls, for the same reason
/// `sessions::RawMode` is one: a process that exits between them leaves the
/// person's terminal with no echo, which reads as a hung machine. This runs
/// on every return path and on unwind.
#[cfg(unix)]
#[derive(Debug)]
struct NoEcho {
    saved: Option<rustix::termios::Termios>,
}

#[cfg(unix)]
impl NoEcho {
    fn enter() -> NoEcho {
        let stdin = std::io::stdin();
        let Ok(saved) = rustix::termios::tcgetattr(&stdin) else {
            return NoEcho { saved: None };
        };
        let mut quiet = saved.clone();
        quiet.local_modes -= rustix::termios::LocalModes::ECHO;
        match rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &quiet) {
            Ok(()) => NoEcho { saved: Some(saved) },
            Err(_) => NoEcho { saved: None },
        }
    }
}

#[cfg(unix)]
impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                &saved,
            );
        }
    }
}

/// `rustix` is a unix-only dependency of this crate. The node is Linux-only
/// anyway (`limits.md` §1), so this exists to keep the crate compiling
/// elsewhere rather than to be used — and it echoes, which is why
/// [`read_passphrase`] is the only caller and says so.
#[cfg(not(unix))]
#[derive(Debug)]
struct NoEcho;

#[cfg(not(unix))]
impl NoEcho {
    fn enter() -> NoEcho {
        NoEcho
    }
}

/// `meshnode identity protect` — seal a plaintext key under a passphrase.
pub fn protect(path: &Path) -> Result<i32> {
    let stored = Stored::read(path).with_context(|| format!("reading {}", path.display()))?;
    let seed = match stored {
        Stored::Sealed(_) => {
            println!("{} is already protected.", path.display());
            println!("To change the passphrase: meshnode identity unprotect, then protect again.");
            return Ok(0);
        }
        Stored::Plain(seed) => seed,
        // Y2/Y3. There is no seed here to seal, and that is the stronger
        // state rather than a gap: a passphrase protects a key that is on
        // this disk, and this one is not.
        Stored::Hardware(_) => {
            println!(
                "{} is bound to a hardware key, so there is nothing on this disk to protect.",
                path.display()
            );
            println!("  meshnode identity hardware status    where the key actually is");
            return Ok(0);
        }
    };

    let mut pass = read_passphrase("New passphrase: ")?;
    // Typed twice, because a key sealed under a typo is a key nobody owns
    // any more — there is no recovery path and no copy of the plaintext.
    let mut again = read_passphrase("Again: ")?;
    let same = pass == again;
    again.zeroize();
    if !same {
        pass.zeroize();
        bail!("those did not match; nothing was changed");
    }

    let sealed = keyfile::seal(&seed, &pass)?;
    // Prove it opens *before* the plaintext stops existing. Sealing and
    // then discovering the file does not decrypt is the one failure this
    // command must not be able to produce.
    let check = Keypair::from_seed(seed);
    let Stored::Sealed(parsed) = Stored::parse_bytes(&sealed)? else {
        pass.zeroize();
        bail!("internal error: freshly sealed key did not parse as sealed");
    };
    let reopened = parsed.open(&pass)?;
    pass.zeroize();
    if Keypair::from_seed(reopened).user_id() != check.user_id() {
        bail!("internal error: the sealed key did not reopen to the same identity");
    }

    keyfile::write_private(path, &sealed).with_context(|| format!("writing {}", path.display()))?;
    println!("{} is now protected.", path.display());
    println!();
    println!("  meshnode identity unlock    once per login session");
    println!("  meshnode identity status    to see what is unlocked");
    println!();
    println!(
        "This protects the key at rest — backups, sync clients, a stolen disk. It does not\n\
         stop a process running as you while the identity is unlocked. See\n\
         docs/operating/limits.md §6."
    );
    Ok(0)
}

/// `meshnode identity unprotect` — go back to a plaintext key.
///
/// Present because a one-way door is a trap: somebody who protects a key on
/// a headless box with no `$XDG_RUNTIME_DIR` needs a way back that is not
/// "restore from a backup you do not have".
pub fn unprotect(path: &Path, yes: bool) -> Result<i32> {
    let stored = Stored::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Stored::Sealed(sealed) = stored else {
        println!("{} is not protected.", path.display());
        return Ok(0);
    };
    if !yes && std::io::stdin().is_terminal() {
        print!(
            "Write {} back as an unencrypted key? [y/N] ",
            path.display()
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was changed.");
            return Ok(1);
        }
    }
    let mut pass = read_passphrase("Passphrase: ")?;
    let seed = sealed.open(&pass);
    pass.zeroize();
    let seed = seed?;
    Keypair::from_seed(seed)
        .persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    keyfile::cache_clear(path);
    println!("{} is now an unencrypted key, mode 0600.", path.display());
    Ok(0)
}

/// `meshnode identity unlock` — open the key for this login session.
pub fn unlock(path: &Path) -> Result<i32> {
    match Stored::read(path).with_context(|| format!("reading {}", path.display()))? {
        Stored::Plain(_) => {
            println!(
                "{} is not protected, so there is nothing to unlock.",
                path.display()
            );
            println!("Run `meshnode identity protect` to put a passphrase on it.");
            Ok(0)
        }
        Stored::Hardware(_) => {
            println!(
                "{} is bound to a hardware key — there is no passphrase, and no unlock to \
                 cache. The agent holding it decides when to ask for a PIN or a touch.",
                path.display()
            );
            Ok(0)
        }
        Stored::Sealed(_) => {
            if let Some(secs) = keyfile::cache_remaining(path) {
                println!("Already unlocked for {}.", humanish(secs));
                return Ok(0);
            }
            let mut pass = read_passphrase("Passphrase: ")?;
            let opened = Keypair::unlock_and_remember(path, &pass);
            pass.zeroize();
            let kp = opened?;
            println!("Unlocked {} as {}.", path.display(), kp.user_id());
            if let Some(secs) = keyfile::cache_remaining(path) {
                println!("This lasts {}, or until you log out.", humanish(secs));
            }
            Ok(0)
        }
    }
}

/// `meshnode identity lock` — forget the unlock now.
pub fn lock(path: &Path) -> Result<i32> {
    let was = keyfile::cache_remaining(path).is_some();
    keyfile::cache_clear(path);
    println!(
        "{}",
        if was {
            format!("Locked {}.", path.display())
        } else {
            format!("{} was not unlocked.", path.display())
        }
    );
    Ok(0)
}

/// `meshnode identity status` — is it protected, is it unlocked, for how long.
pub fn status(path: &Path, json: bool) -> Result<i32> {
    let exists = path.exists();
    let stored = if exists {
        Stored::read(path).ok()
    } else {
        None
    };
    let protected = stored.as_ref().is_some_and(|s| s.is_sealed());
    let remaining = keyfile::cache_remaining(path);

    if json {
        let doc = serde_json::json!({
            "path": path.display().to_string(),
            "exists": exists,
            "protected": protected,
            "unlocked": remaining.is_some(),
            "unlocked_for_secs": remaining,
            "session_store": keyfile::cache_path(path).map(|p| p.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(0);
    }

    println!("key        {}", path.display());
    if !exists {
        println!("state      no key here yet — one is created on first use");
        return Ok(0);
    }
    if stored.is_none() {
        println!("state      unreadable, or not a meshware identity");
        return Ok(1);
    }
    println!(
        "protected  {}",
        if protected {
            "yes — passphrase, Argon2id + XChaCha20-Poly1305"
        } else {
            "no  — a plaintext 32-byte seed. `meshnode identity protect` seals it"
        }
    );
    if protected {
        match remaining {
            Some(secs) => println!("unlocked   yes, for {}", humanish(secs)),
            None => println!("unlocked   no  — `meshnode identity unlock`"),
        }
        match keyfile::cache_path(path) {
            Some(p) => println!("session    {}", p.display()),
            None => println!(
                "session    none — no $XDG_RUNTIME_DIR here, so every command \
                 needs {}",
                keyfile::PASSPHRASE_ENV
            ),
        }
    }
    Ok(0)
}

/// Round a duration to the unit a person would say it in.
fn humanish(secs: u64) -> String {
    match secs {
        0..=90 => format!("{secs}s"),
        91..=5400 => format!("{}m", secs.div_ceil(60)),
        _ => format!("{}h", secs.div_ceil(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(humanish(45), "45s");
        assert_eq!(humanish(600), "10m");
        assert_eq!(humanish(12 * 3600), "12h");
    }

    /// The band edges. Director's `fmtRoughDuration` mirrors this function
    /// so one unlock reads the same in both front ends, and `duration.test.ts`
    /// carries these exact vectors — add a case there when adding one here.
    #[test]
    fn the_bands_switch_where_directors_copy_switches_them() {
        assert_eq!(humanish(90), "90s");
        assert_eq!(humanish(91), "2m");
        assert_eq!(humanish(3_601), "61m");
        assert_eq!(humanish(5_400), "90m");
        assert_eq!(humanish(5_401), "2h");
    }

    /// `protect` must never leave a key it cannot reopen, so it verifies
    /// before it writes. This pins the check that makes that possible.
    #[test]
    fn a_sealed_key_verifies_before_the_plaintext_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        let kp = Keypair::generate().expect("generate");
        kp.persist(&path).expect("persist");

        // Simulate what `protect` does, minus the terminal.
        let Stored::Plain(seed) = Stored::read(&path).expect("read") else {
            panic!("expected a plaintext key");
        };
        let sealed = keyfile::seal(&seed, b"pw").expect("seal");
        let Stored::Sealed(parsed) = Stored::parse_bytes(&sealed).expect("parse") else {
            panic!("expected a sealed key");
        };
        assert_eq!(
            Keypair::from_seed(parsed.open(b"pw").expect("open")).user_id(),
            kp.user_id(),
            "the sealed key must reopen to the same identity"
        );
    }
}
