//! Signing with a key that never leaves the hardware it was made on (Y2/Y3).
//!
//! ## The finding this rests on
//!
//! **A user key in meshware only ever signs.** It is an Ed25519 key that
//! produces 64-byte signatures over operations, records and IPC envelopes,
//! and nothing downstream ever asks it for anything else — the X25519
//! scalar that unwraps secrets and the PKCS#8 export that terminates TLS
//! both come from the *node* key, which is a different key with a different
//! life (landmine 8: do not let this creep toward node keys).
//!
//! So a hardware-held user key needs no protocol change, no second
//! signature algorithm, and no new field anywhere. It needs one thing: a
//! way to ask a device for a signature.
//!
//! ## Why the agent protocol, and not a PIV or PKCS#11 crate
//!
//! Landmine 5 names the trap. The obvious PIV crate reaches the token
//! through `pcsc`, which binds `libpcsclite`; the obvious PKCS#11 route
//! needs `dlopen`, which a static musl binary cannot do at all. Both
//! collide with the rule that got `jsonwebtoken` refused in `oidc.rs` and
//! `unsafe-libyaml` refused in `Cargo.toml`: this node is a static musl
//! binary with no C toolchain.
//!
//! The landmine offers two ways out — keep hardware out of the daemon, or
//! load PKCS#11 at runtime. There is a third, and it is better than both:
//! **the thing that talks to the hardware is already running, and it
//! already speaks a protocol over a Unix socket.**
//!
//! - `gpg-agent --enable-ssh-support` exposes an OpenPGP card's
//!   authentication subkey. That is **Y3**, on hardware that has had
//!   Ed25519 since firmware 5.2.3 — the wide-fleet answer.
//! - `ssh-add -s` with a PKCS#11 module (OpenSC's or Yubico's) exposes an
//!   Ed25519 key in a PIV slot. That is **Y2**, for firmware 5.7 and later.
//!   Slot 9a, made with a PIN policy of `once`: 9c's default is a PIN for
//!   every signature, and every request is signed. (`yubikey-agent` makes
//!   ECDSA keys, which cannot be a meshware identity.)
//! - A resident FIDO2 credential and a plain key file also arrive this way.
//!
//! One protocol, both backends, no C, no `dlopen`, and the hardware code
//! stays in a process that was written to own it. What this module links is
//! a Unix socket and a length-prefixed binary format.
//!
//! ## What this proves, and what it does not
//!
//! **An agent proves nothing about where a key lives.** It will sign with a
//! key on a YubiKey and with a key in `~/.ssh/id_ed25519` and the two
//! answers are identical bytes. Anyone who reads "hardware" into the mere
//! presence of an agent has been told something this module cannot know.
//!
//! That is what [`crate::attest`] is for, and why it is a separate,
//! replicated, merge-verified record rather than a flag somebody's client
//! sets. The agent is how a signature is *obtained*; the attestation
//! certificate is the only thing that says it could not have been copied.
//!
//! ## Touch fatigue (landmine 1)
//!
//! [`Keypair::sign_request`](crate::identity::Keypair::sign_request) fires
//! on every IPC call — `status`, `nodes`, a tab completion. A backend that
//! asked for a touch on each one would be uninstalled the same afternoon.
//! Nothing here caches anything, deliberately: the agent already owns that
//! decision and is the right place for it. `gpg-agent` has
//! `default-cache-ttl`, PIV has a `cached` touch policy that covers fifteen
//! seconds of signing, and `yubikey-agent` has its own. Adding a fourth
//! cache here would mean a lifetime to argue about and a second place for
//! it to be wrong. `meshnode identity hardware status` prints what the
//! agent says so the answer is visible rather than assumed.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CoreError, Result};

/// How long to wait on the agent for one answer.
///
/// Long, because the wait is a person: a PIV key with a `cached` touch
/// policy asks for a finger, and `gpg-agent` may put a pinentry on screen.
/// Short enough that a wedged agent is an error rather than a hang.
const AGENT_TIMEOUT: Duration = Duration::from_secs(120);

/// Refuse anything larger than this from the agent. An agent is trusted to
/// hold a key, not to be well-behaved about frame sizes.
const MAX_AGENT_FRAME: u32 = 256 * 1024;

// SSH agent protocol message numbers (RFC 4251 §4.2 / the OpenSSH
// PROTOCOL.agent file).
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENT_SIGN_RESPONSE: u8 = 14;
const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENT_SUCCESS: u8 = 6;
const SSH_AGENTC_ADD_SMARTCARD_KEY: u8 = 20;
const SSH_AGENTC_REMOVE_SMARTCARD_KEY: u8 = 21;

/// The one key type this speaks.
///
/// **`sk-ssh-ed25519@openssh.com` is deliberately excluded**, even though a
/// resident FIDO2 credential is a perfectly good hardware key. Its
/// signature is not a bare Ed25519 signature: OpenSSH wraps the
/// authenticator's flags and counter alongside it and signs
/// `SHA256(data)` under the authenticator's own framing. Accepting one here
/// would mean either a second verification path in every node or a
/// signature that verifies nowhere — and meshware already has a first-class
/// answer for touch-backed authorization in `presence.rs` (Y5), which is
/// where that hardware belongs.
const SSH_ED25519: &str = "ssh-ed25519";

/// A key held somewhere this process cannot read (Y2/Y3).
#[derive(Clone)]
pub struct HardwareKey {
    socket: PathBuf,
    /// The raw 32-byte Ed25519 public key, which is also the `UserId`.
    public: [u8; 32],
    /// The agent's own comment for it — a path, a card serial, a label.
    /// Shown to people; never trusted for anything.
    comment: String,
}

impl std::fmt::Debug for HardwareKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HardwareKey(pub={}, agent={})",
            mesh_proto::UserId(self.public).short(),
            self.socket.display()
        )
    }
}

impl HardwareKey {
    pub fn public_bytes(&self) -> [u8; 32] {
        self.public
    }

    pub fn user_id(&self) -> mesh_proto::UserId {
        mesh_proto::UserId(self.public)
    }

    /// What the agent calls this key. For a screen, never for a decision.
    pub fn comment(&self) -> &str {
        &self.comment
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A key bound to an agent that is not there, for testing the paths
    /// that must not reach one: the exports that fail closed, and the
    /// refusal a missing token produces.
    #[cfg(test)]
    pub(crate) fn unreachable_for_test(public: [u8; 32]) -> HardwareKey {
        HardwareKey {
            socket: PathBuf::from("/nonexistent/meshware-test-agent.sock"),
            public,
            comment: "test token".into(),
        }
    }

    /// Ask the agent to sign.
    ///
    /// **Fails closed** (landmine 3). `load_or_generate` deliberately
    /// creates a key for an unenrolled person so they are refused by name
    /// rather than by a permissions error; that behaviour under a hardware
    /// binding would silently downgrade an admin to a software key the
    /// moment their token was unplugged. There is no fallback path here
    /// and there is no flag to add one: if the agent cannot produce the
    /// signature, the command fails and says why.
    pub fn sign(&self, msg: &[u8]) -> Result<[u8; 64]> {
        let mut blob = Vec::new();
        put_string(&mut blob, SSH_ED25519.as_bytes());
        put_string(&mut blob, &self.public);

        let mut payload = Vec::new();
        put_string(&mut payload, &blob);
        put_string(&mut payload, msg);
        put_u32(&mut payload, 0); // no flags: ed25519 has no rsa-sha2 variants

        let reply = self.round_trip(SSH_AGENTC_SIGN_REQUEST, &payload)?;
        let (kind, body) = split_reply(&reply)?;
        if kind == SSH_AGENT_FAILURE {
            return Err(CoreError::Identity(format!(
                "the agent at {} refused to sign with your key. It is loaded but would not use \
                 it — a token that has been unplugged, a PIN that was not entered, or a touch \
                 that was not given in time",
                self.socket.display()
            )));
        }
        if kind != SSH_AGENT_SIGN_RESPONSE {
            return Err(CoreError::Identity(format!(
                "the agent answered a sign request with message type {kind}, which is not a \
                 signature"
            )));
        }
        parse_signature(body)
    }

    fn round_trip(&self, kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|e| {
            CoreError::Identity(format!(
                "cannot reach the signing agent at {}: {e}. Is it running, and is \
                 SSH_AUTH_SOCK pointing at it?",
                self.socket.display()
            ))
        })?;
        stream
            .set_read_timeout(Some(AGENT_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(AGENT_TIMEOUT)))
            .map_err(|e| CoreError::Identity(format!("configuring the agent socket: {e}")))?;

        let mut frame = Vec::with_capacity(payload.len() + 5);
        put_u32(
            &mut frame,
            u32::try_from(payload.len() + 1).unwrap_or(u32::MAX),
        );
        frame.push(kind);
        frame.extend_from_slice(payload);
        stream
            .write_all(&frame)
            .map_err(|e| CoreError::Identity(format!("writing to the signing agent: {e}")))?;

        let mut len = [0u8; 4];
        stream.read_exact(&mut len).map_err(|e| {
            CoreError::Identity(format!(
                "the signing agent closed the connection without answering: {e}"
            ))
        })?;
        let len = u32::from_be_bytes(len);
        if len == 0 || len > MAX_AGENT_FRAME {
            return Err(CoreError::Identity(format!(
                "the signing agent announced a {len}-byte reply, which is not plausible"
            )));
        }
        let mut body = vec![0u8; len as usize];
        stream
            .read_exact(&mut body)
            .map_err(|e| CoreError::Identity(format!("reading from the signing agent: {e}")))?;
        Ok(body)
    }
}

/// Where the agent socket is, in the order a person means it.
///
/// `$MESHWARE_AGENT_SOCK` first, so a mesh identity can live on a different
/// agent from the one `ssh` uses — which is the common case for anybody who
/// runs `gpg-agent` for a card and `ssh-agent` for everything else. Then
/// `$SSH_AUTH_SOCK`, which is what is already set on nearly every machine.
pub fn agent_socket(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    for var in ["MESHWARE_AGENT_SOCK", "SSH_AUTH_SOCK"] {
        if let Ok(value) = std::env::var(var)
            && !value.is_empty()
        {
            return Ok(PathBuf::from(value));
        }
    }
    Err(CoreError::Identity(
        "no signing agent: neither MESHWARE_AGENT_SOCK nor SSH_AUTH_SOCK is set.\n  \
         For an OpenPGP card:  export SSH_AUTH_SOCK=$(gpgconf --list-dirs agent-ssh-socket)\n  \
         For PIV slot 9c:      ssh-add -s /usr/lib/libykcs11.so"
            .into(),
    ))
}

/// Every Ed25519 key the agent is holding.
///
/// Keys of other types are skipped rather than reported, because an agent
/// full of RSA keys is the normal state of a working machine and has
/// nothing to do with whether a mesh identity is there.
pub fn list(socket: &Path) -> Result<Vec<HardwareKey>> {
    let probe = HardwareKey {
        socket: socket.to_path_buf(),
        public: [0u8; 32],
        comment: String::new(),
    };
    let reply = probe.round_trip(SSH_AGENTC_REQUEST_IDENTITIES, &[])?;
    let (kind, body) = split_reply(&reply)?;
    if kind != SSH_AGENT_IDENTITIES_ANSWER {
        return Err(CoreError::Identity(format!(
            "the agent at {} answered a list request with message type {kind}",
            socket.display()
        )));
    }
    Ok(parse_identities(body)?
        .into_iter()
        .map(|(public, comment)| HardwareKey {
            socket: socket.to_path_buf(),
            public,
            comment,
        })
        .collect())
}

/// The Ed25519 keys in an agent's identities answer. Pure, so the bytes
/// another process sends can be fuzzed (SEC-22).
pub(crate) fn parse_identities(body: &[u8]) -> Result<Vec<([u8; 32], String)>> {
    let mut r = Reader::new(body);
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let blob = r.string()?;
        let comment = r.string()?;
        let mut b = Reader::new(blob);
        let Ok(alg) = b.string() else { continue };
        if alg != SSH_ED25519.as_bytes() {
            continue;
        }
        let Ok(key) = b.string() else { continue };
        let Ok(public) = <[u8; 32]>::try_from(key) else {
            continue;
        };
        out.push((public, String::from_utf8_lossy(comment).into_owned()));
    }
    Ok(out)
}

/// The signature in an agent's sign answer (see [`HardwareKey::sign`]).
pub(crate) fn parse_signature(body: &[u8]) -> Result<[u8; 64]> {
    let mut r = Reader::new(body);
    let signature = r.string()?;
    // The signature blob is itself framed: an algorithm name and then
    // the bytes. Checked rather than assumed — an agent that answered
    // with an rsa-sha2-512 signature over an ed25519 key blob would
    // otherwise produce 64 bytes of something from the middle of a
    // 512-byte field.
    let mut sr = Reader::new(signature);
    let alg = sr.string()?;
    if alg != SSH_ED25519.as_bytes() {
        return Err(CoreError::Identity(format!(
            "the agent signed with {}, and meshware identities are Ed25519",
            String::from_utf8_lossy(alg)
        )));
    }
    let raw = sr.string()?;
    <[u8; 64]>::try_from(raw).map_err(|_| {
        CoreError::Identity(format!(
            "the agent returned a {}-byte Ed25519 signature, which is not one",
            raw.len()
        ))
    })
}

// ---------------------------------------------------------------------------
// where an agent's key lives, as far as this machine can tell
// ---------------------------------------------------------------------------
//
// An agent cannot say whether a key is on a token or in a file, and nothing
// here pretends it can. But this machine can say one thing for certain: when
// a key is one of the files in `~/.ssh`. GNOME's agent offers every key it
// finds there, so on a desktop the list a person picks from is mostly files —
// and binding to one of those, believing it to be a security key, protects
// nothing while saying it does (2026-09-18: exactly that happened). So a key
// that matches a `.pub` file is named as the file it is, and binding to one
// is refused unless forced.

/// Where an agent's key comes from, as far as this machine can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOrigin {
    /// A public key file in `~/.ssh` matches it: the private half is a file
    /// on this computer. The path is the private key's — the `.pub` minus
    /// its extension — because that is the file that matters.
    File(PathBuf),
    /// The agent's comment names a smart card or a PKCS#11 module: a key an
    /// agent loaded from a device. Likely, not proven — only an attestation
    /// proves it ([`crate::attest`]).
    Device,
    /// Neither: not a file this machine can find, and nothing in the
    /// comment to go on.
    Unknown,
}

impl HardwareKey {
    /// Where this key lives, as far as this machine can tell. Reads
    /// `~/.ssh/*.pub`; asks the agent nothing.
    pub fn origin(&self) -> KeyOrigin {
        origin_in(ssh_dir().as_deref(), &self.public, &self.comment)
    }
}

fn ssh_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".ssh"))
}

/// [`HardwareKey::origin`], against a directory of public keys.
pub fn origin_in(dir: Option<&Path>, public: &[u8; 32], comment: &str) -> KeyOrigin {
    if let Some(file) = dir.and_then(|d| file_holding(d, public)) {
        return KeyOrigin::File(file);
    }
    let c = comment.to_ascii_lowercase();
    // What the agents that front a device call their keys: OpenSC's PIV
    // labels ("PIV AUTH key"), a PKCS#11 module path, gpg-agent's
    // "cardno:…", yubikey-agent's "YubiKey #… PIV Slot 9a".
    let device = [
        "piv",
        "cardno:",
        "yubikey",
        "pkcs11",
        "pkcs#11",
        ".so",
        "smart card",
    ]
    .iter()
    .any(|m| c.contains(m));
    if device {
        KeyOrigin::Device
    } else {
        KeyOrigin::Unknown
    }
}

/// The private key file whose `.pub` in `dir` is `public`, if any.
fn file_holding(dir: &Path, public: &[u8; 32]) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut pubs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pub"))
        .collect();
    pubs.sort();
    pubs.into_iter().find_map(|p| {
        // A public key is a line; anything large is not one.
        if std::fs::metadata(&p).map_or(true, |m| m.len() > 16 * 1024) {
            return None;
        }
        let text = std::fs::read_to_string(&p).ok()?;
        text.lines()
            .any(|l| ssh_line_is(l, public))
            .then(|| p.with_extension(""))
    })
}

/// Is `line` an OpenSSH `ssh-ed25519` public key line for `public`?
fn ssh_line_is(line: &str, public: &[u8; 32]) -> bool {
    let mut parts = line.split_whitespace();
    if parts.next() != Some(SSH_ED25519) {
        return false;
    }
    let Some(blob) = parts
        .next()
        .and_then(|b| data_encoding::BASE64.decode(b.as_bytes()).ok())
    else {
        return false;
    };
    let mut r = Reader::new(&blob);
    matches!(r.string(), Ok(alg) if alg == SSH_ED25519.as_bytes())
        && matches!(r.string(), Ok(key) if key == public.as_slice())
}

// ---------------------------------------------------------------------------
// connecting a smart card
// ---------------------------------------------------------------------------

/// The PKCS#11 modules that can reach a YubiKey's PIV applet, in the order
/// to prefer them: Yubico's own, then OpenSC's, in the places distributions
/// put them.
const PKCS11_MODULES: &[&str] = &[
    "/usr/lib64/libykcs11.so",
    "/usr/lib/x86_64-linux-gnu/libykcs11.so",
    "/usr/lib/aarch64-linux-gnu/libykcs11.so",
    "/usr/lib/libykcs11.so",
    "/usr/local/lib/libykcs11.so",
    "/usr/lib64/opensc-pkcs11.so",
    "/usr/lib/x86_64-linux-gnu/opensc-pkcs11.so",
    "/usr/lib/aarch64-linux-gnu/opensc-pkcs11.so",
    "/usr/lib/opensc-pkcs11.so",
    "/usr/local/lib/opensc-pkcs11.so",
];

/// The PKCS#11 module to load a security key through, if this machine has
/// one. Resolved to its real path: `ssh-agent` only loads modules from an
/// allowed list of directories, and checks the real one.
pub fn pkcs11_module() -> Option<PathBuf> {
    PKCS11_MODULES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// Load the keys on a smart card into the agent, unlocking the card with
/// its PIN — `ssh-add -e` and then `ssh-add -s <module>`, without a terminal
/// to type the PIN into.
///
/// The module is removed first, and whether that worked is not asked:
/// OpenSSH's agent refuses to add a module it already holds, and one it
/// holds from before the card was last unplugged has a dead session — its
/// keys are listed and every signature fails. Removing and adding is how
/// a card is reconnected.
///
/// **Every call spends a PIN attempt when the PIN is wrong**, and a PIV
/// applet locks its PIN after three. Nothing here retries.
pub fn connect_smartcard(socket: &Path, module: &Path, pin: &str) -> Result<()> {
    let probe = HardwareKey {
        socket: socket.to_path_buf(),
        public: [0u8; 32],
        comment: String::new(),
    };
    let module_bytes = module.as_os_str().as_encoded_bytes();
    let mut remove = Vec::new();
    put_string(&mut remove, module_bytes);
    put_string(&mut remove, b"");
    let _ = probe.round_trip(SSH_AGENTC_REMOVE_SMARTCARD_KEY, &remove);

    let mut add = Vec::new();
    put_string(&mut add, module_bytes);
    put_string(&mut add, pin.as_bytes());
    let reply = probe.round_trip(SSH_AGENTC_ADD_SMARTCARD_KEY, &add)?;
    match split_reply(&reply)?.0 {
        SSH_AGENT_SUCCESS => Ok(()),
        SSH_AGENT_FAILURE => Err(CoreError::Identity(
            "the key agent could not open the security key. The PIN may be wrong, the key may \
             not be plugged in, or the agent may not be allowed to load smart cards"
                .into(),
        )),
        other => Err(CoreError::Identity(format!(
            "the key agent answered with message type {other}, which is neither yes nor no"
        ))),
    }
}

/// The one key in the agent whose public bytes are `want`.
///
/// Looked up by the identity itself rather than by comment or position: a
/// comment is whatever the agent felt like saying, and a position changes
/// when somebody runs `ssh-add`. The `UserId` **is** the public key, so
/// this is the only lookup that cannot silently bind the wrong key.
pub fn find(socket: &Path, want: &[u8; 32]) -> Result<HardwareKey> {
    let keys = list(socket)?;
    keys.into_iter().find(|k| &k.public == want).ok_or_else(|| {
        CoreError::Identity(format!(
            "the agent at {} is not holding {}. Plug the token in, or load the key: \
                 `ssh-add -s /usr/lib64/opensc-pkcs11.so` (or libykcs11.so) for PIV, or set \
                 SSH_AUTH_SOCK to `gpgconf --list-dirs agent-ssh-socket` for an OpenPGP card",
            socket.display(),
            mesh_proto::UserId(*want).short(),
        ))
    })
}

// ---------------------------------------------------------------------------
// binding an account to a key
// ---------------------------------------------------------------------------
//
// `identity hardware use` used to write the reference over the key file and
// then print advice. Two things were wrong with that, and they compounded.
//
// A key made on a device is a *new* identity. Writing its reference over a
// file that held a key the mesh still honoured destroyed the only copy of
// that key, and the guard meant to stop it asked whether the *new* key was
// the sole admin — which a key the mesh has never heard of never is. A
// founder with one admin key could lock themselves out of their own mesh in
// one command.
//
// And the advice named `users rotate --to`, a flag that does not exist,
// after the key that could have run it was already gone. For the founder
// there was no right advice to give: the founding key can be neither
// rotated nor revoked.
//
// So the decision is made here, before anything is written, from the same
// user rows `meshnode users` prints; and the file is changed by
// [`bind_file`], which moves a key aside rather than ever overwriting one
// that still means something. The CLI and Director both come through here.

/// Why binding this account to a key was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindRefusal {
    /// The key is not a member of this mesh, and the key this account uses
    /// now is: binding would make this machine a stranger to its own mesh.
    NotAMember {
        /// The name the current key goes by.
        username: String,
        /// The current key is the founding admin's, which can never be
        /// rotated, so the way across is a second admin rather than
        /// `users rotate`.
        founder: bool,
        /// The current key holds admin, so it can publish the rotation
        /// itself rather than asking somebody to.
        admin: bool,
    },
    /// The key would be the only admin left, and it is on a token nobody
    /// else can replace: a lost token would be a lost mesh.
    SoleAdmin,
}

/// What binding does to the key this account holds now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldKey {
    /// It is the key being bound. The file changes; the identity does not.
    Same,
    /// The mesh no longer honours it, or never did. Replacing it loses
    /// nothing.
    Spent,
    /// The mesh still honours it — or nobody could ask. It is moved aside,
    /// never overwritten: a key is the one file nobody can make again.
    Live,
}

/// Decide whether this account may switch from `current` to `chosen`, from
/// the mesh's own user rows. Nothing is written.
///
/// **An old key that is set aside still counts as an admin** for the
/// sole-admin question, because it is one: it is honoured, and it is
/// exactly the key that can rotate this person back in if the token is
/// lost. That is the founder's position after binding a second admin key —
/// the founding key goes in a drawer and stays the way back.
pub fn check_bind(
    current: mesh_proto::UserId,
    chosen: mesh_proto::UserId,
    users: &[mesh_proto::UserRow],
) -> std::result::Result<OldKey, BindRefusal> {
    let live = |id: mesh_proto::UserId| users.iter().find(|u| u.user_id == id && !u.revoked);
    let admin = |u: &mesh_proto::UserRow| u.roles.iter().any(|r| r == "admin");

    let old = if current == chosen {
        OldKey::Same
    } else if live(current).is_some() {
        OldKey::Live
    } else {
        OldKey::Spent
    };
    if old == OldKey::Live && live(chosen).is_none() {
        let row = live(current).map(|u| (u.username.clone(), u.is_founder, admin(u)));
        let (username, founder, admin) = row.unwrap_or_default();
        return Err(BindRefusal::NotAMember {
            username,
            founder,
            admin,
        });
    }
    let admins: Vec<_> = users.iter().filter(|u| !u.revoked && admin(u)).collect();
    if let [only] = admins.as_slice()
        && only.user_id == chosen
    {
        return Err(BindRefusal::SoleAdmin);
    }
    Ok(old)
}

/// The identity in the key file at `path`, read from that file itself.
/// `None` when there is no file.
///
/// Not the key a client would sign with, which can come from a different
/// file (the CLI falls back to this account's own key when the node's is
/// unreadable): the question is what binding would replace, and that is
/// this file. A sealed key opens only with the session's unlock or
/// `$MESHWARE_PASSPHRASE`, and a locked one is an error — never a guess.
/// A hardware reference is read without asking its agent anything.
pub fn identity_in_file(path: &Path) -> Result<Option<mesh_proto::UserId>> {
    if !path.exists() {
        return Ok(None);
    }
    let public = match crate::keyfile::Stored::read(path)? {
        crate::keyfile::Stored::Hardware(public) => public,
        crate::keyfile::Stored::Plain(_) | crate::keyfile::Stored::Sealed(_) => {
            crate::identity::Keypair::load_or_generate(path)?
                .0
                .public_bytes()
        }
    };
    Ok(Some(mesh_proto::UserId(public)))
}

/// Point the key file at `chosen`, moving aside what was there when it
/// still means something. Returns where the old key went, if it moved.
///
/// Only a file that might hold a secret is moved. One that already points
/// at hardware holds a public key and a note, so replacing it loses
/// nothing.
///
/// The old key keeps whatever protection it had: a sealed file is renamed,
/// not opened, so it stays sealed.
pub fn bind_file(
    path: &Path,
    chosen: &[u8; 32],
    old: OldKey,
    old_id: Option<mesh_proto::UserId>,
) -> Result<Option<PathBuf>> {
    // Anything but a hardware reference might be a secret — a file that
    // will not parse included, which is a reason for care, not for
    // overwriting it.
    let secret = path.exists()
        && !matches!(
            crate::keyfile::Stored::read(path),
            Ok(crate::keyfile::Stored::Hardware(_))
        );
    let moved = if old == OldKey::Live && secret {
        let to = set_aside_path(path, old_id);
        std::fs::rename(path, &to).map_err(|e| {
            CoreError::Identity(format!(
                "could not move {} aside to {}: {e} — nothing was changed",
                path.display(),
                to.display()
            ))
        })?;
        Some(to)
    } else {
        None
    };
    crate::keyfile::write_private(path, &crate::keyfile::hardware_ref(chosen))?;
    // The unlock cache belongs to a key that is no longer at this path.
    crate::keyfile::cache_clear(path);
    Ok(moved)
}

/// `user.key.set-aside-mwu1…`, beside the key file, never over an existing
/// file: a second binding must not bury the key the first one set aside.
fn set_aside_path(path: &Path, old_id: Option<mesh_proto::UserId>) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "user.key".into());
    let tag = old_id.map_or_else(|| "previous".to_string(), |id| id.short());
    let base = path.with_file_name(format!("{name}.set-aside-{tag}"));
    let mut candidate = base.clone();
    let mut n = 2;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}-{n}", base.display()));
        n += 1;
    }
    candidate
}

/// Keys that binding set aside beside `path`, with the short id each one's
/// name carries (`user.key.set-aside-mwu1…`). Newest first by name order
/// is not meaningful, so they come back sorted and the caller matches on
/// the id.
pub fn set_aside_keys(path: &Path) -> Vec<(PathBuf, String)> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.set-aside-", name.to_string_lossy());
    let mut found: Vec<(PathBuf, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let tag = file.strip_prefix(&prefix)?;
            // `-2`, `-3`… is a second binding's; the id is what precedes it.
            let id = match tag.rsplit_once('-') {
                Some((id, n)) if n.chars().all(|c| c.is_ascii_digit()) => id,
                _ => tag,
            };
            Some((e.path(), id.to_string()))
        })
        .collect();
    found.sort();
    found
}

/// Undo a binding: put a key [`bind_file`] set aside back at `path`.
///
/// Only ever over a hardware reference (or nothing): that is the one thing
/// at `path` that holds no secret, so this cannot do what binding was
/// changed never to do — overwrite a key. The restored key keeps whatever
/// protection it had; a sealed key comes back sealed, and locked.
pub fn restore_file(path: &Path, set_aside: &Path) -> Result<()> {
    let expected = path
        .file_name()
        .map(|n| format!("{}.set-aside-", n.to_string_lossy()));
    let named_right = set_aside.parent() == path.parent()
        && set_aside
            .file_name()
            .zip(expected)
            .is_some_and(|(n, e)| n.to_string_lossy().starts_with(&e));
    if !named_right {
        return Err(CoreError::Identity(format!(
            "{} is not a key that binding set aside beside {}",
            set_aside.display(),
            path.display()
        )));
    }
    if path.exists()
        && !matches!(
            crate::keyfile::Stored::read(path),
            Ok(crate::keyfile::Stored::Hardware(_))
        )
    {
        return Err(CoreError::Identity(format!(
            "{} holds a key of its own, not a reference to a security key — nothing was \
             restored over it",
            path.display()
        )));
    }
    std::fs::rename(set_aside, path).map_err(|e| {
        CoreError::Identity(format!(
            "could not move {} back to {}: {e} — nothing was changed",
            set_aside.display(),
            path.display()
        ))
    })?;
    crate::keyfile::cache_clear(path);
    Ok(())
}

// ---------------------------------------------------------------------------
// wire helpers
// ---------------------------------------------------------------------------

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, u32::try_from(s.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(s);
}

fn split_reply(reply: &[u8]) -> Result<(u8, &[u8])> {
    match reply.split_first() {
        Some((kind, rest)) => Ok((*kind, rest)),
        None => Err(CoreError::Identity(
            "the signing agent sent an empty reply".into(),
        )),
    }
}

/// A cursor over the agent's length-prefixed fields.
///
/// Every read is bounds-checked and returns an error rather than panicking:
/// this parses bytes from another process, and `unwrap_used` is a lint in
/// this workspace for exactly the shape of code that would otherwise live
/// here.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, at: 0 }
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let end = self.at.saturating_add(4);
        let slice = self
            .buf
            .get(self.at..end)
            .ok_or_else(|| CoreError::Identity("truncated reply from the signing agent".into()))?;
        self.at = end;
        let bytes: [u8; 4] = slice
            .try_into()
            .map_err(|_| CoreError::Identity("truncated reply from the signing agent".into()))?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// One raw byte. SSHSIG's `sk-` tail carries a bare flags byte rather
    /// than a length-prefixed field (Y6).
    pub(crate) fn byte(&mut self) -> Result<u8> {
        let b = self
            .buf
            .get(self.at)
            .copied()
            .ok_or_else(|| CoreError::Identity("truncated signature".into()))?;
        self.at += 1;
        Ok(b)
    }

    pub(crate) fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        let end = self.at.saturating_add(len);
        let slice = self
            .buf
            .get(self.at..end)
            .ok_or_else(|| CoreError::Identity("truncated reply from the signing agent".into()))?;
        self.at = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing, round-tripped. Getting a length prefix wrong here does
    /// not produce a wrong signature — it produces a hang on a socket,
    /// which is much harder to read.
    #[test]
    fn strings_round_trip_through_the_reader() {
        let mut buf = Vec::new();
        put_string(&mut buf, b"ssh-ed25519");
        put_string(&mut buf, &[7u8; 32]);
        put_u32(&mut buf, 9);

        let mut r = Reader::new(&buf);
        assert_eq!(r.string().expect("alg"), b"ssh-ed25519");
        assert_eq!(r.string().expect("key"), &[7u8; 32]);
        assert_eq!(r.u32().expect("flags"), 9);
    }

    /// A short buffer is an error, never a panic and never a silent zero.
    /// These bytes come from another process.
    #[test]
    fn a_truncated_field_is_an_error() {
        let mut buf = Vec::new();
        put_u32(&mut buf, 64); // claims 64 bytes
        buf.extend_from_slice(&[1u8; 8]); // provides 8

        let mut r = Reader::new(&buf);
        assert!(r.string().is_err());

        let mut short = Reader::new(&[0u8, 1]);
        assert!(short.u32().is_err(), "two bytes is not a u32");
    }

    /// A length that would overflow a `usize` add must be refused rather
    /// than wrapping into a small, plausible-looking slice.
    #[test]
    fn an_absurd_length_does_not_wrap() {
        let mut buf = Vec::new();
        put_u32(&mut buf, u32::MAX);
        buf.push(0);
        assert!(Reader::new(&buf).string().is_err());
    }

    /// **The whole path, against a real agent — a *disposable* one.**
    ///
    /// `MESHWARE_TEST_AGENT_SOCK` and nothing else. It deliberately does
    /// **not** fall back to `agent_socket(None)`, which would pick up
    /// `SSH_AUTH_SOCK` and ask whoever is running the suite to unlock their
    /// personal key so a test can sign a fixed string with it. A test that
    /// reaches for whatever credential happens to be loaded is not a test,
    /// it is a prompt somebody did not ask for — and on a desktop agent it
    /// is indistinguishable from something going wrong.
    ///
    /// Start one, put a throwaway key in it, and run this deliberately:
    ///
    /// ```sh
    /// eval "$(ssh-agent -s)"
    /// ssh-keygen -q -t ed25519 -N "" -f /tmp/mw-test-key
    /// ssh-add /tmp/mw-test-key
    /// MESHWARE_TEST_AGENT_SOCK="$SSH_AUTH_SOCK" \
    ///   cargo test -p mesh-core --lib live_agent -- --ignored --nocapture
    /// ssh-agent -k
    /// ```
    ///
    /// A unit test over the framing proves the parser agrees with itself.
    /// This proves it agrees with OpenSSH — which is the distinction Y5's
    /// three real bugs were all about.
    #[test]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    fn live_agent_round_trip() {
        let Some(sock) = std::env::var_os("MESHWARE_TEST_AGENT_SOCK") else {
            // Skipped rather than failed, and skipped *loudly*: the one
            // thing this must never do is quietly start using a different
            // agent because this one was not set.
            eprintln!(
                "\n  skipped: set MESHWARE_TEST_AGENT_SOCK to a throwaway agent's socket.\n  \
                 This test will not use SSH_AUTH_SOCK — see the comment above it.\n"
            );
            return;
        };
        let socket = PathBuf::from(sock);
        let keys = list(&socket).expect("list identities");
        eprintln!(
            "\n  agent {} holds {} Ed25519 key(s)",
            socket.display(),
            keys.len()
        );
        assert!(!keys.is_empty(), "load a throwaway key into that agent");

        let key = &keys[0];
        eprintln!("  signing with {} ({})", key.user_id(), key.comment());
        let message = b"meshware hardware signer round trip";
        let signature = key.sign(message).expect("the agent must sign");

        assert!(
            crate::identity::verify_signature(&key.public_bytes(), message, &signature),
            "the agent's signature must verify against the key it advertised"
        );
        assert!(
            !crate::identity::verify_signature(&key.public_bytes(), b"something else", &signature),
            "and must not verify against anything else"
        );
        eprintln!("  verified, and refused on different bytes\n");
    }

    fn row(id: u8, name: &str, roles: &[&str], founder: bool) -> mesh_proto::UserRow {
        mesh_proto::UserRow {
            user_id: mesh_proto::UserId([id; 32]),
            username: name.into(),
            is_founder: founder,
            revoked: false,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            created_at_micros: 0,
            hardware: None,
        }
    }

    fn uid(id: u8) -> mesh_proto::UserId {
        mesh_proto::UserId([id; 32])
    }

    /// The lockout this exists to prevent: the only admin, holding the
    /// founding key, binds a key made fresh on a device. The mesh has never
    /// heard of that key, so the old guard — "is the *new* key the sole
    /// admin?" — said no and the founding key was overwritten.
    #[test]
    fn a_founder_cannot_bind_a_key_the_mesh_does_not_know() {
        let users = [row(1, "ada", &["admin"], true)];
        assert_eq!(
            check_bind(uid(1), uid(9), &users),
            Err(BindRefusal::NotAMember {
                username: "ada".into(),
                founder: true,
                admin: true,
            })
        );
    }

    /// The way across for a founder: the device key joins as a second
    /// admin, and the founding key — still honoured, never revocable — is
    /// set aside rather than overwritten. It is also why this is not a
    /// sole-admin refusal: the founding key is the other admin.
    #[test]
    fn a_founder_binding_a_second_admin_key_sets_the_founding_key_aside() {
        let users = [
            row(1, "ada", &["admin"], true),
            row(9, "ada-key", &["admin"], false),
        ];
        assert_eq!(check_bind(uid(1), uid(9), &users), Ok(OldKey::Live));
    }

    /// After `users rotate`, the old key is revoked: replacing it loses
    /// nothing, so it is not kept.
    #[test]
    fn a_rotated_key_is_spent() {
        let mut old = row(2, "mia", &[], false);
        old.revoked = true;
        let users = [
            row(1, "ada", &["admin"], true),
            old,
            row(9, "mia", &["operator"], false),
        ];
        assert_eq!(check_bind(uid(2), uid(9), &users), Ok(OldKey::Spent));
    }

    /// Somebody not yet in any mesh can put their identity on a token
    /// before enrolling. Their current key is nobody's, so it is spent.
    #[test]
    fn a_key_nobody_knows_is_spent_and_binding_is_allowed() {
        let users = [row(1, "ada", &["admin"], true)];
        assert_eq!(check_bind(uid(5), uid(9), &users), Ok(OldKey::Spent));
    }

    /// A founding key imported onto a token binds as the same identity —
    /// and with nobody else holding admin, a lost token is a lost mesh.
    #[test]
    fn the_only_admin_on_a_token_is_refused() {
        let users = [row(1, "ada", &["admin"], true)];
        assert_eq!(
            check_bind(uid(1), uid(1), &users),
            Err(BindRefusal::SoleAdmin)
        );
        let users = [
            row(1, "ada", &["admin"], true),
            row(3, "bo", &["admin"], false),
        ];
        assert_eq!(check_bind(uid(1), uid(1), &users), Ok(OldKey::Same));
    }

    /// A non-admin whose key the mesh knows is told to ask an admin, which
    /// the refusal carries so each front end can say it its own way.
    #[test]
    fn a_member_who_is_not_an_admin_is_told_so() {
        let users = [
            row(1, "ada", &["admin"], true),
            row(2, "mia", &["operator"], false),
        ];
        assert_eq!(
            check_bind(uid(2), uid(9), &users),
            Err(BindRefusal::NotAMember {
                username: "mia".into(),
                founder: false,
                admin: false,
            })
        );
    }

    /// A live key is renamed, byte for byte, and a second binding does not
    /// bury the first one's.
    #[test]
    fn binding_moves_a_live_key_aside_and_never_over_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        let old = crate::identity::Keypair::from_seed([7u8; 32]);
        std::fs::write(&path, [7u8; 32]).expect("write");

        let moved = bind_file(&path, &[9u8; 32], OldKey::Live, Some(old.user_id()))
            .expect("bind")
            .expect("a live key moves");
        assert_eq!(std::fs::read(&moved).expect("read"), [7u8; 32]);
        assert!(moved.to_string_lossy().contains(&old.user_id().short()));
        assert!(matches!(
            crate::keyfile::Stored::read(&path),
            Ok(crate::keyfile::Stored::Hardware(p)) if p == [9u8; 32]
        ));

        std::fs::write(&path, [8u8; 32]).expect("write");
        let again = bind_file(&path, &[9u8; 32], OldKey::Live, Some(old.user_id()))
            .expect("bind")
            .expect("moves");
        assert_ne!(again, moved, "the first set-aside key survives");
        assert_eq!(std::fs::read(&moved).expect("read"), [7u8; 32]);
    }

    /// A spent key, and a file that only ever pointed at hardware, are
    /// replaced in place.
    #[test]
    fn binding_replaces_a_spent_key_and_a_hardware_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        std::fs::write(&path, [7u8; 32]).expect("write");
        assert_eq!(
            bind_file(&path, &[9u8; 32], OldKey::Spent, None).expect("bind"),
            None
        );
        assert_eq!(
            bind_file(&path, &[4u8; 32], OldKey::Live, None).expect("bind"),
            None,
            "a hardware reference holds no secret, so nothing is set aside"
        );
        assert_eq!(std::fs::read_dir(dir.path()).expect("ls").count(), 1);
    }

    fn ssh_pub_line(public: &[u8; 32], comment: &str) -> String {
        let mut blob = Vec::new();
        put_string(&mut blob, SSH_ED25519.as_bytes());
        put_string(&mut blob, public);
        format!(
            "{SSH_ED25519} {} {comment}\n",
            data_encoding::BASE64.encode(&blob)
        )
    }

    /// 2026-09-18: a person picked `neuralpunk@NIM` from the list believing
    /// it was their YubiKey. It was `~/.ssh/meshware`. A key that matches a
    /// `.pub` file is that file, and says so.
    #[test]
    fn a_key_that_is_a_file_in_ssh_is_named_as_that_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("meshware.pub"),
            ssh_pub_line(&[3u8; 32], "neuralpunk@NIM"),
        )
        .expect("write");
        std::fs::write(dir.path().join("other.pub"), ssh_pub_line(&[4u8; 32], "x")).expect("write");
        assert_eq!(
            origin_in(Some(dir.path()), &[3u8; 32], "neuralpunk@NIM"),
            KeyOrigin::File(dir.path().join("meshware"))
        );
        // A comment that looks like a card does not outrank the file.
        assert_eq!(
            origin_in(Some(dir.path()), &[4u8; 32], "PIV AUTH key"),
            KeyOrigin::File(dir.path().join("other"))
        );
    }

    #[test]
    fn a_key_no_file_matches_is_a_device_only_when_the_agent_says_card() {
        let dir = tempfile::tempdir().expect("tempdir");
        for comment in [
            "PIV AUTH key",
            "cardno:000F_1234",
            "YubiKey #31145069 PIV Slot 9a",
            "/usr/lib64/opensc-pkcs11.so",
        ] {
            assert_eq!(
                origin_in(Some(dir.path()), &[5u8; 32], comment),
                KeyOrigin::Device,
                "{comment}"
            );
        }
        assert_eq!(
            origin_in(Some(dir.path()), &[5u8; 32], "laptop"),
            KeyOrigin::Unknown
        );
        assert_eq!(origin_in(None, &[5u8; 32], "laptop"), KeyOrigin::Unknown);
    }

    #[test]
    fn a_malformed_pub_file_matches_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("bad.pub"),
            "ssh-ed25519 !!!not-base64!!! x\nssh-rsa AAAA y\n",
        )
        .expect("write");
        assert_eq!(
            origin_in(Some(dir.path()), &[6u8; 32], "x"),
            KeyOrigin::Unknown
        );
    }

    /// Going back puts the set-aside key where it was, over the reference
    /// that replaced it — and over nothing that holds a secret.
    #[test]
    fn restoring_puts_the_set_aside_key_back_over_a_reference_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        std::fs::write(&path, [7u8; 32]).expect("write");
        let aside = bind_file(
            &path,
            &[9u8; 32],
            OldKey::Live,
            Some(mesh_proto::UserId([7u8; 32])),
        )
        .expect("bind")
        .expect("set aside");
        let listed = set_aside_keys(&path);
        assert_eq!(
            listed,
            vec![(aside.clone(), mesh_proto::UserId([7u8; 32]).short())]
        );

        restore_file(&path, &aside).expect("restore");
        assert_eq!(std::fs::read(&path).expect("read"), vec![7u8; 32]);
        assert!(!aside.exists());

        // Now `path` is a real key again: nothing is restored over it.
        let again = dir.path().join(format!(
            "user.key.set-aside-{}",
            mesh_proto::UserId([8u8; 32]).short()
        ));
        std::fs::write(&again, [8u8; 32]).expect("write");
        assert!(restore_file(&path, &again).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), vec![7u8; 32]);

        // And only a file binding set aside, beside this key, is restored.
        let stray = dir.path().join("user.key.stray");
        std::fs::write(&stray, [1u8; 32]).expect("write");
        std::fs::write(&path, crate::keyfile::hardware_ref(&[9u8; 32])).expect("write");
        assert!(restore_file(&path, &stray).is_err());
    }

    #[test]
    fn a_second_set_aside_key_is_listed_by_its_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        std::fs::write(dir.path().join("user.key.set-aside-mwu1abc"), b"x").expect("write");
        std::fs::write(dir.path().join("user.key.set-aside-mwu1abc-2"), b"x").expect("write");
        std::fs::write(dir.path().join("user.key.from-director"), b"x").expect("write");
        let ids: Vec<String> = set_aside_keys(&path)
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        assert_eq!(ids, vec!["mwu1abc".to_string(), "mwu1abc".into()]);
    }

    /// The smart-card messages, against a real agent — a *disposable* one,
    /// as [`live_agent_round_trip`] explains. A module the agent cannot
    /// load is a refusal in words, and the agent goes on working.
    #[test]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    fn live_a_module_the_agent_cannot_load_is_refused_cleanly() {
        let Some(sock) = std::env::var_os("MESHWARE_TEST_AGENT_SOCK") else {
            eprintln!("\n  skipped: set MESHWARE_TEST_AGENT_SOCK to a throwaway agent's socket.\n");
            return;
        };
        let socket = PathBuf::from(sock);
        let err = connect_smartcard(&socket, Path::new("/nonexistent/pkcs11.so"), "123456")
            .expect_err("no such module");
        assert!(
            err.to_string().contains("could not open the security key"),
            "{err}"
        );
        list(&socket).expect("the agent still answers");
    }

    #[test]
    fn the_socket_comes_from_the_meshware_variable_first() {
        // The explicit path always wins, and needs no environment at all.
        let explicit = PathBuf::from("/tmp/explicit.sock");
        assert_eq!(
            agent_socket(Some(&explicit)).expect("explicit"),
            explicit,
            "a path passed in is not negotiable"
        );
    }
}
