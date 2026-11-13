//! Node and user identity: Ed25519 keypairs. The key *is* the identity —
//! no CA, no certificates to manage (MASTERPLAN §4.1, §5).
//!
//! On-disk format: 32 raw seed bytes, mode 0600, under `<data_dir>/identity/`
//! — or, for a *user* key that somebody has run `meshnode identity protect`
//! on, the sealed envelope in [`crate::keyfile`]. Both are read by
//! [`Keypair::load`]; only the plaintext one is ever written here.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use mesh_proto::{IpcRequest, NodeId, SignedIpc, UserId, ipc_signing_bytes};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{CoreError, Result};

const NODE_KEY_FILE: &str = "node.key";
/// The CLI/SDK user key inside a home's `identity/` directory. Public
/// because the job executor points `MESHWARE_USER_IDENTITY` at it when
/// it spawns a task, so a task that talks back to the mesh signs with
/// the same key the CLI on that node signs with.
pub const USER_KEY_FILE: &str = "user.key";

/// Where [`load_cli_user`] will look, for a surface that has to *name* the
/// file rather than just read it — "your key lives here" is half of what
/// `meshnode whoami` owes an unenrolled person (M23 §4).
pub fn cli_user_identity_path(data_dir: &Path, explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    // The same precedence [`load_cli_user`] signs with, minus the creating.
    // These two must not disagree: the whole job of this function is to
    // tell a person which file holds the key their commands are signed
    // with, and naming a file in the daemon's home to somebody who cannot
    // read it is worse than saying nothing.
    let daemon_user = Identity::identity_dir(data_dir).join(USER_KEY_FILE);
    if std::fs::File::open(&daemon_user).is_ok() {
        return daemon_user;
    }
    for own in [
        default_user_identity_path(),
        crate::layout::Layout::new(crate::layout::default_home())
            .identity_dir()
            .join(USER_KEY_FILE),
    ] {
        if own != daemon_user && std::fs::File::open(&own).is_ok() {
            return own;
        }
    }
    daemon_user
}

/// An Ed25519 identity, either held here or held by hardware (Y2/Y3).
///
/// One type with two insides, rather than a trait threaded through every
/// signing site in the tree, and the reason is the finding Part Two rests
/// on: **a user key in meshware only ever signs.** Every caller of this
/// type wants `sign`, `public_bytes` or `user_id`, and those three mean
/// exactly the same thing whichever variant is underneath. The handful of
/// operations that genuinely need private *bytes* — the X25519 scalar that
/// unwraps secrets, the PKCS#8 export that terminates TLS, the seed a
/// service account is handed — belong to the **node** key and to key
/// creation, never to a person's key on a token, and each of them now says
/// so rather than silently doing the wrong thing (landmine 8).
#[derive(Clone)]
pub struct Keypair {
    inner: KeyInner,
}

#[derive(Clone)]
enum KeyInner {
    /// A seed this process holds. Everything, including the exports.
    Software(SigningKey),
    /// A key an agent holds on hardware this process cannot read. Signs,
    /// and refuses the rest by name (Y2/Y3).
    Hardware(crate::hardware::HardwareKey),
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Keypair({}, pub={})",
            match self.inner {
                KeyInner::Software(_) => "software",
                KeyInner::Hardware(_) => "hardware",
            },
            NodeId(self.public_bytes()).short()
        )
    }
}

impl Keypair {
    pub fn generate() -> Result<Self> {
        // Seeds are wiped where they are dropped (SEC-23): the signing key
        // wipes its own copy, and every other copy made on the way to it is
        // this function's to clean up.
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut *seed)
            .map_err(|e| CoreError::Identity(format!("system rng unavailable: {e}")))?;
        Ok(Self {
            inner: KeyInner::Software(SigningKey::from_bytes(&seed)),
        })
    }

    /// A key from its seed. The copy handed in is wiped here; the caller
    /// wipes its own (`Zeroizing`), since an array is copied, not moved.
    pub fn from_seed(mut seed: [u8; 32]) -> Self {
        let key = Self {
            inner: KeyInner::Software(SigningKey::from_bytes(&seed)),
        };
        seed.zeroize();
        key
    }

    /// Bind this identity to a key an agent holds (Y2/Y3).
    ///
    /// The `UserId` is unchanged — it *is* the public key, and the public
    /// key is the same one whether the private half lives in a file or on a
    /// card. That is what makes moving an existing identity onto hardware a
    /// key-location change rather than an enrolment: every grant, every
    /// signature already in the log, and every reference to this person
    /// keeps meaning what it meant.
    pub fn from_hardware(key: crate::hardware::HardwareKey) -> Self {
        Self {
            inner: KeyInner::Hardware(key),
        }
    }

    /// Is the private half somewhere this process cannot read?
    pub fn is_hardware(&self) -> bool {
        matches!(self.inner, KeyInner::Hardware(_))
    }

    /// The agent-held key behind this identity, if there is one.
    pub fn hardware(&self) -> Option<&crate::hardware::HardwareKey> {
        match &self.inner {
            KeyInner::Hardware(h) => Some(h),
            KeyInner::Software(_) => None,
        }
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        match &self.inner {
            KeyInner::Software(k) => k.verifying_key().to_bytes(),
            KeyInner::Hardware(h) => h.public_bytes(),
        }
    }

    /// This person's release key: the minisign key their published releases
    /// are signed with (`crate::release::ReleaseSigner`). Derived from this
    /// key, so there is nothing new to store, back up or lose.
    ///
    /// `None` for a hardware-held identity, whose private half cannot be
    /// read to derive from. Such a person signs a release with `minisign`
    /// and publishes the `.minisig` beside it, as before.
    pub fn release_signer(&self) -> Option<crate::release::ReleaseSigner> {
        match &self.inner {
            KeyInner::Software(k) => Some(crate::release::ReleaseSigner::derive(&k.to_bytes())),
            KeyInner::Hardware(_) => None,
        }
    }

    pub fn node_id(&self) -> NodeId {
        NodeId(self.public_bytes())
    }

    pub fn user_id(&self) -> UserId {
        UserId(self.public_bytes())
    }

    /// Sign, or **panic** for a hardware key — use [`Keypair::try_sign`].
    ///
    /// This is the shape the tree already had: `sign` is infallible and is
    /// called in well over a hundred places, most of them building a record
    /// inline where a `?` has nowhere to go. Changing all of them would be
    /// a refactor with one real bug hiding in it.
    ///
    /// So the fallible path is the new name and this one keeps the old
    /// contract, with a panic that cannot be reached by a hardware key
    /// arriving somewhere it should not: [`load_cli_user`] is the only
    /// function that produces one, the CLI routes every signature through
    /// [`Keypair::try_sign`], and `debug_assert` in tests catches a new
    /// caller on the old path. A daemon or a node key is never hardware
    /// (landmine 8), so the daemon's own signing is unaffected.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        match &self.inner {
            KeyInner::Software(k) => k.sign(msg).to_bytes(),
            KeyInner::Hardware(_) => self
                .try_sign(msg)
                .unwrap_or_else(|e| panic!("hardware signature required but unavailable: {e}")),
        }
    }

    /// Sign, saying why not (Y2/Y3).
    ///
    /// **Fails closed** (landmine 3). A hardware key whose token is
    /// unplugged is an error naming the token, never a quiet fall back to a
    /// software key — which is the specific way `load_or_generate`'s
    /// fail-open behaviour would have gone wrong under a hardware policy.
    pub fn try_sign(&self, msg: &[u8]) -> Result<[u8; 64]> {
        match &self.inner {
            KeyInner::Software(k) => Ok(k.sign(msg).to_bytes()),
            KeyInner::Hardware(h) => h.sign(msg),
        }
    }

    /// The clamped X25519 scalar of this Ed25519 key, for unwrapping secret
    /// data keys (MASTERPLAN §7.3 envelope encryption).
    ///
    /// **Node keys only, and now enforced rather than assumed.** Secrets
    /// are wrapped to the *node* that will run the work, never to a
    /// person's key, so this is never reached with a hardware identity —
    /// and no token exports a scalar, so if it ever were, the honest answer
    /// is an error rather than a key derived from the wrong half.
    pub fn to_x25519_scalar_bytes(&self) -> [u8; 32] {
        match &self.inner {
            KeyInner::Software(k) => k.to_scalar_bytes(),
            KeyInner::Hardware(_) => panic!(
                "a hardware-held key has no exportable X25519 scalar; secrets are wrapped to node keys, which are never hardware-held"
            ),
        }
    }

    /// Sign an IPC request into its transport envelope (MASTERPLAN §7.1:
    /// every API request is signed by the user key — no passwords, no
    /// bearer tokens).
    pub fn sign_request(&self, req: &IpcRequest) -> Result<SignedIpc> {
        self.sign_request_inner(req, None)
    }

    /// [`Self::sign_request`], bound to one mesh and one node (SEC-12).
    /// What a request over the HTTP API has to be: it is refused at every
    /// other node, and by a node of any other mesh.
    pub fn sign_request_for(
        &self,
        req: &IpcRequest,
        audience: &mesh_proto::Audience,
    ) -> Result<SignedIpc> {
        self.sign_request_inner(req, Some(audience))
    }

    fn sign_request_inner(
        &self,
        req: &IpcRequest,
        audience: Option<&mesh_proto::Audience>,
    ) -> Result<SignedIpc> {
        let payload = postcard::to_stdvec(req)
            .map_err(|e| CoreError::Identity(format!("encode request: {e}")))?;
        let mut nonce_bytes = [0u8; 8];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|e| CoreError::Identity(format!("system rng unavailable: {e}")))?;
        let nonce = u64::from_le_bytes(nonce_bytes);
        let user = self.user_id();
        let at_micros = mesh_proto::now_micros();
        // `try_sign`, not `sign`: this is the one function every CLI and
        // Director request passes through, so it is where a hardware key
        // whose token is unplugged has to produce a sentence rather than a
        // panic (landmine 3 — fail closed, and say why).
        let preimage = match audience {
            Some(audience) => {
                mesh_proto::ipc_signing_bytes_for(audience, &user, at_micros, nonce, &payload)
            }
            None => ipc_signing_bytes(&user, at_micros, nonce, &payload),
        };
        let signature = self.try_sign(&preimage)?.to_vec();
        Ok(SignedIpc {
            user,
            at_micros,
            nonce,
            payload,
            signature,
        })
    }

    /// PKCS#8 DER of the private key, for handing to rcgen/rustls.
    ///
    /// **Node keys only.** TLS termination needs the private half in this
    /// process; a token will not export one, and a node key is never
    /// hardware-held (landmine 8 — the node key also derives an X25519
    /// scalar, so putting it on a TPM is a different project). Reported as
    /// an error rather than assumed away.
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let KeyInner::Software(signing) = &self.inner else {
            return Err(CoreError::Identity(
                "a hardware-held key cannot be exported to terminate TLS — node identities are \
                 never hardware-held"
                    .into(),
            ));
        };
        let doc = signing
            .to_pkcs8_der()
            .map_err(|e| CoreError::Identity(format!("pkcs8 encode: {e}")))?;
        Ok(doc.as_bytes().to_vec())
    }

    /// Load a key, sealed or not.
    ///
    /// A sealed key opens from the session unlock in `$XDG_RUNTIME_DIR`, or
    /// from `MESHWARE_PASSPHRASE` where there is no session to speak of. It
    /// never prompts: this is called from library code that may be running
    /// under a daemon, inside a job, or behind Director's UI, and a blocking
    /// read on stdin in any of those is a hang rather than a question. The
    /// CLI prompts and then calls [`Keypair::unlock_and_remember`].
    fn load(path: &Path) -> Result<Self> {
        match crate::keyfile::Stored::read(path)? {
            crate::keyfile::Stored::Plain(seed) => {
                let seed = Zeroizing::new(seed);
                Ok(Self::from_seed(*seed))
            }
            crate::keyfile::Stored::Sealed(sealed) => {
                if let Some(kek) = crate::keyfile::cache_get(path) {
                    let kek = Zeroizing::new(kek);
                    let seed = Zeroizing::new(sealed.open_with_kek(&kek)?);
                    return Ok(Self::from_seed(*seed));
                }
                if let Some(pass) = std::env::var_os(crate::keyfile::PASSPHRASE_ENV) {
                    let pass = Zeroizing::new(pass.into_encoded_bytes());
                    let seed = Zeroizing::new(sealed.open(&pass)?);
                    return Ok(Self::from_seed(*seed));
                }
                Err(CoreError::IdentityLocked(path.display().to_string()))
            }
            // Y2/Y3. The key is on a card; the agent is asked for it now
            // rather than at the first signature, so "your token is not
            // plugged in" arrives before a command has done half its work.
            crate::keyfile::Stored::Hardware(public) => {
                let socket = crate::hardware::agent_socket(None)?;
                Ok(Self::from_hardware(crate::hardware::find(
                    &socket, &public,
                )?))
            }
        }
    }

    /// Open a sealed key with a passphrase and remember the unlock for the
    /// rest of the login session.
    ///
    /// The unlock kept is the *key-encryption key*, never the seed — so the
    /// thing sitting in the runtime directory cannot sign, and is useless
    /// without the sealed file it belongs to.
    pub fn unlock_and_remember(path: &Path, passphrase: &[u8]) -> Result<Self> {
        let crate::keyfile::Stored::Sealed(sealed) = crate::keyfile::Stored::read(path)? else {
            return Err(CoreError::Identity(format!(
                "{} is not sealed — there is nothing to unlock",
                path.display()
            )));
        };
        let kek = Zeroizing::new(sealed.kek(passphrase)?);
        // Open before caching. Caching first would remember a wrong
        // passphrase and turn one typo into a session of confusing failures.
        let seed = Zeroizing::new(sealed.open_with_kek(&kek)?);
        crate::keyfile::cache_put(path, &kek)?;
        Ok(Self::from_seed(*seed))
    }

    /// The raw 32-byte seed.
    ///
    /// Deliberately narrow and deliberately awkward to reach: the only
    /// caller is `meshnode serviceaccounts create`, which has to hand a
    /// freshly minted machine credential to a person exactly once (E1-P4).
    /// Nothing else in the tree needs a private key as bytes — signing goes
    /// through [`Keypair::sign`] and persistence through
    /// [`Keypair::persist`] — so a second caller appearing is a review
    /// question rather than a convenience.
    ///
    /// **Software keys only**, and the refusal is part of the feature: a
    /// service account minted from a token would be a machine credential
    /// nobody could ever hand over, and a token that *did* export its seed
    /// was never non-exportable in the first place.
    pub fn seed_bytes(&self) -> Result<Zeroizing<[u8; 32]>> {
        match &self.inner {
            KeyInner::Software(k) => Ok(Zeroizing::new(k.to_bytes())),
            KeyInner::Hardware(_) => Err(CoreError::Identity(
                "a hardware-held key has no seed to export — that is what makes it hardware. \
                 Service accounts are minted as software keys (E1-P4); a job has no finger to \
                 press (landmine 2)"
                    .into(),
            )),
        }
    }

    /// Persist the 32-byte seed at `path`, mode 0600. Public so embedders
    /// (Director) can mirror one human identity across key locations
    /// (e.g. `~/.meshware/identity` ↔ the engine data dir's `user.key`).
    pub fn persist(&self, path: &Path) -> Result<()> {
        let seed = self.seed_bytes()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // **Created 0600, not created and then made 0600** (S-L1). The two
        // steps left a window — small, and real on a shared machine — in
        // which a private key existed at `0666 & ~umask`.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            use std::os::unix::fs::PermissionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(&*seed)?;
            file.sync_all()?;
            // `mode` only applies to a file this call *created*; one that
            // was already there keeps whatever it had, so say it again.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        std::fs::write(path, &*seed)?;
        Ok(())
    }

    /// Load the key at `path`, creating one if there is nothing there.
    ///
    /// The `bool` is "this was just created". Note what this does **not**
    /// do: a file that exists and cannot be read — sealed and locked,
    /// corrupt, unreadable — is an error, never a reason to mint a second
    /// identity over the top of the first. Generating there would hand
    /// somebody a brand-new key with none of their grants and no hint that
    /// their real one is still sitting on disk.
    pub fn load_or_generate(path: &Path) -> Result<(Self, bool)> {
        if path.exists() {
            Ok((Self::load(path)?, false))
        } else {
            let kp = Self::generate()?;
            kp.persist(path)?;
            Ok((kp, true))
        }
    }
}

/// Verify a detached Ed25519 signature against a raw public key.
/// Which audience a request envelope's signature was made for (SEC-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcBinding {
    /// `mw-ipc-v1`: no audience at all.
    Unbound,
    /// `mw-ipc-v2`, for this node of the mesh the caller named.
    Bound(NodeId),
}

/// Does `env` carry a valid signature by its user — unbound, or bound to
/// `mesh` and one of `nodes`? And which? (SEC-12)
///
/// The verifier supplies the audiences it would accept: itself for a
/// request made to it, the origin as well for work an origin forwarded.
pub fn verify_ipc(
    env: &SignedIpc,
    mesh: mesh_proto::MeshId,
    nodes: &[NodeId],
) -> Option<IpcBinding> {
    let sig = <[u8; 64]>::try_from(env.signature.as_slice()).ok()?;
    for node in nodes {
        let audience = mesh_proto::Audience { mesh, node: *node };
        let preimage = mesh_proto::ipc_signing_bytes_for(
            &audience,
            &env.user,
            env.at_micros,
            env.nonce,
            &env.payload,
        );
        if verify_signature(&env.user.0, &preimage, &sig) {
            return Some(IpcBinding::Bound(*node));
        }
    }
    let preimage = ipc_signing_bytes(&env.user, env.at_micros, env.nonce, &env.payload);
    verify_signature(&env.user.0, &preimage, &sig).then_some(IpcBinding::Unbound)
}

pub fn verify_signature(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    vk.verify(msg, &Signature::from_bytes(sig)).is_ok()
}

/// The identities a node holds: the node key (machine identity) and, on a
/// node bootstrapped as a mesh founder, the founder's user key.
#[derive(Debug, Clone)]
pub struct Identity {
    pub node: Keypair,
    /// The founder's key, when this machine holds one **and it can be
    /// opened**.
    ///
    /// `None` on every node that did not found the mesh — the ordinary case
    /// for all but one machine — and also when the file is present but
    /// sealed (B9). From the daemon's point of view those two are the same
    /// state: there is no key here it can sign as a human with.
    ///
    /// It has to be an `Option` rather than a load failure because **a
    /// daemon has nobody to ask for a passphrase.** `meshnode up` under
    /// systemd starts at boot with no terminal and, for a system service, no
    /// `$XDG_RUNTIME_DIR` to hold an unlock either. A sealed user key must
    /// therefore never stop a node booting; it only means this node cannot
    /// author the founder's own records, which is already true everywhere
    /// else in the fleet.
    pub user: Option<Keypair>,
    pub node_created: bool,
    pub user_created: bool,
}

impl Identity {
    pub fn identity_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("identity")
    }

    /// Load this node's keys, creating either if absent.
    ///
    /// The node key is required and a failure to read it is fatal — without
    /// it there is no node. The user key is not: a sealed one yields
    /// [`Identity::user`] `= None` and the node comes up, because the
    /// alternative is a machine that will not boot after somebody protected
    /// their key on their workstation. Every other read error on it is
    /// still an error, so a corrupt file is not quietly ignored.
    pub fn load_or_generate(data_dir: &Path) -> Result<Self> {
        let dir = Self::identity_dir(data_dir);
        let (node, node_created) = Keypair::load_or_generate(&dir.join(NODE_KEY_FILE))?;
        let (user, user_created) = match Keypair::load_or_generate(&dir.join(USER_KEY_FILE)) {
            Ok((kp, created)) => (Some(kp), created),
            Err(CoreError::IdentityLocked(_)) => (None, false),
            Err(e) => return Err(e),
        };
        Ok(Self {
            node,
            user,
            node_created,
            user_created,
        })
    }
}

/// Default location of the *CLI* user identity: `<home>/identity/user.key`
/// (MASTERPLAN §7.1). Passphrase encryption of this file is a planned
/// refinement; today it is a mode-0600 seed like the node key.
///
/// This used to be `~/.meshware/identity` — a *file* at the path the home's
/// identity *directory* now occupies. [`crate::layout::Layout::ensure`]
/// resolves that collision on first run; nothing here has to know about it.
pub fn default_user_identity_path() -> PathBuf {
    crate::layout::Layout::resolve(None)
        .identity_dir()
        .join(USER_KEY_FILE)
}

/// Resolve the CLI's user identity. Precedence:
/// 1. an explicit path (flag or `MESHWARE_USER_IDENTITY`),
/// 2. the local daemon's stored admin key (`<home>/identity/user.key`) if
///    **this account can read it** — so the founder's machine is the admin
///    with zero setup,
/// 3. this person's own key under their own home, created if absent;
///    commands are signed and then refused by name until an admin adds
///    them.
///
/// Steps 2 and 3 used to be the same file, and that is the bug this
/// function now exists to not have. A packaged node runs as its own system
/// user out of `/var/lib/meshware` with `identity/` mode 0700, so for
/// *every other account on that machine* step 2 is `EACCES` — and the old
/// code turned that into a hard stop reading
///
/// ```text
/// error: loading user identity: io error: Permission denied (os error 13)
/// ```
///
/// which describes a file nobody asked about, in a directory nobody
/// mentioned, and says nothing whatever about the actual situation: *you
/// are not a member of this mesh*. Not being able to read the daemon's
/// private key is the normal, correct state for an unauthorized user. So
/// it is no longer an error at all — the CLI signs with the caller's own
/// identity and lets the node answer, which is the only place the question
/// "may this person do this" is ever really decided (MASTERPLAN §7.1).
pub fn load_cli_user(data_dir: &Path, explicit: Option<&Path>) -> Result<Keypair> {
    load_cli_user_from(data_dir, explicit, &own_identity_paths())
}

/// Where this *account* keeps a key of its own, best first.
///
/// Two candidates, and the second is the one that matters: a packaged
/// install commonly exports `MESHWARE_HOME=/var/lib/meshware` for everyone
/// on the box, which is the very directory an unprivileged account cannot
/// read — so "the resolved home" and "this account's own home" become the
/// same unusable path, and only `~/.meshware` is left.
fn own_identity_paths() -> Vec<PathBuf> {
    let mut paths = vec![default_user_identity_path()];
    let bare = crate::layout::Layout::new(crate::layout::default_home())
        .identity_dir()
        .join(USER_KEY_FILE);
    if !paths.contains(&bare) {
        paths.push(bare);
    }
    paths
}

/// [`load_cli_user`] with the fallback locations passed in, so the
/// unreadable-home path can be tested without writing to the environment.
fn load_cli_user_from(
    data_dir: &Path,
    explicit: Option<&Path>,
    own_paths: &[PathBuf],
) -> Result<Keypair> {
    if let Some(path) = explicit {
        return Keypair::load_or_generate(path)
            .map(|(kp, _)| kp)
            .map_err(|e| match e {
                // Already the right message, and already names the file.
                // Wrapping it would bury the two commands that fix it.
                locked @ CoreError::IdentityLocked(_) => locked,
                e => CoreError::Identity(format!(
                    "cannot use the identity at {}: {e}",
                    path.display()
                )),
            });
    }
    let daemon_user = Identity::identity_dir(data_dir).join(USER_KEY_FILE);
    // **A locked key stops here, and so does an unavailable hardware one**
    // ([`stops_resolution`]). Every other failure below falls through to the
    // next candidate, which is right for "this account cannot read the
    // daemon's key" — the case this function was written for. It is exactly
    // wrong for "the key is there and cannot be used right now": falling
    // through would sign with some other key, or create a fresh one under
    // this person's own home, and get them refused by name — reporting that
    // they are not a member of a mesh they administer, while their real
    // identity sits unread. Fail closed and say which file it is.
    match Keypair::load_or_generate(&daemon_user) {
        Ok((kp, _)) => return Ok(kp),
        Err(e) if stops_resolution(&daemon_user, &e) => return Err(e),
        Err(_) => {}
    }
    // The node's home is not ours to read or write. Fall back to this
    // person's own identity, which is exactly what an unenrolled human on
    // a machine with a system daemon should be signing with.
    for own in own_paths {
        if *own == daemon_user {
            continue;
        }
        match Keypair::load_or_generate(own) {
            Ok((kp, _)) => return Ok(kp),
            Err(e) if stops_resolution(own, &e) => return Err(e),
            Err(_) => {}
        }
    }
    Err(CoreError::Identity(format!(
        "no meshware identity this account can use.\n\n\
         {} belongs to the account the node runs as, and a key of your own could not be \
         created under {}.\n\n\
         Point MESHWARE_USER_IDENTITY at a file you can write, or run the CLI as the \
         account the node runs as.",
        daemon_user.display(),
        crate::layout::default_home().display()
    )))
}

/// Does this failure to load `path` end the search, rather than fall
/// through to the next candidate?
///
/// Two failures do, and they are the same failure: the key is **there**
/// and cannot be used right now. A sealed key with no unlock, and a key
/// file that points at hardware whose agent cannot produce it — the token
/// unplugged, the agent not running, `SSH_AUTH_SOCK` unset. Falling through
/// on either signs as somebody else. For the hardware case that was a live
/// bug: with the token out, the CLI quietly signed as whatever other key
/// it could open — this account's own, unlocked by the session — and
/// would have minted a fresh one where there was none, which is exactly
/// the silent fallback to a software key landmine 3 forbids.
///
/// Anything else — the node's home unreadable to this account, above all
/// — still falls through, because that is the unenrolled person this
/// resolver exists for.
fn stops_resolution(path: &Path, e: &CoreError) -> bool {
    matches!(e, CoreError::IdentityLocked(_))
        || matches!(
            crate::keyfile::Stored::read(path),
            Ok(crate::keyfile::Stored::Hardware(_))
        )
}

#[cfg(test)]
mod tests {
    // -- hardware-held identities (Y2/Y3) -----------------------------------

    /// A key file pointing at hardware that cannot sign ends the search.
    /// Asked of the rule rather than through the resolver, so the test
    /// never touches whatever agent the environment happens to name.
    #[test]
    fn an_unavailable_hardware_key_stops_the_resolver() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hw = tmp.path().join("hw.key");
        crate::keyfile::write_private(&hw, &crate::keyfile::hardware_ref(&[9u8; 32]))
            .expect("write");
        let plain = tmp.path().join("plain.key");
        std::fs::write(&plain, [7u8; 32]).expect("write");
        let gone = CoreError::Identity("the agent is not holding it".into());

        assert!(stops_resolution(&hw, &gone), "a missing token must stop");
        assert!(
            !stops_resolution(&plain, &gone),
            "an ordinary unreadable key still falls through"
        );
        assert!(stops_resolution(
            &plain,
            &CoreError::IdentityLocked("x".into())
        ));
    }

    fn hardware_key() -> Keypair {
        Keypair::from_hardware(crate::hardware::HardwareKey::unreachable_for_test(
            [9u8; 32],
        ))
    }

    /// The public half is the identity, and it reads the same whether the
    /// private half is here or on a card. That is what makes moving onto a
    /// token a key-location change rather than an enrolment.
    #[test]
    fn a_hardware_identity_answers_the_public_questions_normally() {
        let kp = hardware_key();
        assert!(kp.is_hardware());
        assert_eq!(kp.public_bytes(), [9u8; 32]);
        assert_eq!(kp.user_id(), UserId([9u8; 32]));
        assert_eq!(kp.node_id(), NodeId([9u8; 32]));
        assert!(kp.hardware().is_some());
    }

    /// **Landmine 3: fail closed.** `load_or_generate` deliberately mints a
    /// key for an unenrolled person so they are refused by name rather than
    /// by a permissions error. That same behaviour under a hardware binding
    /// would quietly downgrade an admin to a software key the moment their
    /// token was unplugged. There is no fallback and no flag to add one.
    #[test]
    fn signing_without_the_token_is_an_error_that_names_the_agent() {
        let kp = hardware_key();
        let err = kp.try_sign(b"anything").expect_err("must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("agent"),
            "the refusal has to point at the thing that is missing: {msg}"
        );
    }

    /// The three operations that need private *bytes* belong to the node
    /// key and to key creation, never to a person's key on a token — and
    /// each now says so rather than doing something plausible and wrong
    /// (landmine 8).
    #[test]
    fn the_exports_refuse_a_hardware_key_by_name() {
        let kp = hardware_key();
        let seed = kp.seed_bytes().expect_err("no seed to export");
        assert!(seed.to_string().contains("hardware"), "{seed}");
        let tls = kp.to_pkcs8_der().expect_err("no private half to export");
        assert!(tls.to_string().contains("hardware"), "{tls}");
        // `persist` is `seed_bytes` plus a write, so it refuses too — and
        // must, or a "backup your key" flow would write a 32-byte file
        // that signs nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(kp.persist(&dir.path().join("user.key")).is_err());
    }

    /// A hardware identity's key file holds a *public* key and says so.
    /// Round-tripped here because the three forms are told apart by their
    /// first six bytes, and getting that wrong would make an older build
    /// try to derive a signing key from a public one.
    #[test]
    fn the_hardware_key_file_round_trips_and_holds_no_secret() {
        let public = [7u8; 32];
        let bytes = crate::keyfile::hardware_ref(&public);
        match crate::keyfile::Stored::parse_bytes(&bytes).expect("parse") {
            crate::keyfile::Stored::Hardware(got) => assert_eq!(got, public),
            other => panic!("expected a hardware reference, got {other:?}"),
        }
        // Not a plaintext seed, and not a sealed key: those two forms must
        // never claim these bytes.
        assert_ne!(bytes.len(), 32, "a hardware reference is not a bare seed");
    }

    use super::*;

    /// **The daemon must boot with a sealed user key.**
    ///
    /// `meshnode up` under systemd starts at boot with no terminal to prompt
    /// on and, as a system service, no `$XDG_RUNTIME_DIR` to hold an unlock.
    /// Before this was an `Option`, protecting a key on a workstation whose
    /// home is also a node home stopped that node starting — a regression
    /// with no message pointing at the cause.
    #[test]
    fn a_node_boots_with_a_sealed_user_key_and_no_founder_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("node");
        let id_dir = Identity::identity_dir(&data_dir);
        std::fs::create_dir_all(&id_dir).expect("identity dir");

        let node = Keypair::generate().expect("node key");
        node.persist(&id_dir.join(NODE_KEY_FILE))
            .expect("persist node");
        let human = Keypair::generate().expect("user key");
        let sealed =
            crate::keyfile::seal(&human.seed_bytes().expect("software key"), b"pw").expect("seal");
        crate::keyfile::write_private(&id_dir.join(USER_KEY_FILE), &sealed).expect("write");

        let identity =
            Identity::load_or_generate(&data_dir).expect("a sealed key must not stop a boot");
        assert_eq!(
            identity.node.node_id(),
            node.node_id(),
            "the node key still loads"
        );
        assert!(
            identity.user.is_none(),
            "a sealed user key is the same as no founder key, from the daemon's side"
        );
        // And it must not have quietly minted a replacement over the top.
        assert_eq!(
            std::fs::read(id_dir.join(USER_KEY_FILE))
                .expect("read")
                .len(),
            105,
            "the sealed key must be left exactly as it was"
        );
    }

    /// A corrupt key is still an error. Only *locked* degrades to `None`.
    #[test]
    fn a_corrupt_user_key_is_still_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("node");
        let id_dir = Identity::identity_dir(&data_dir);
        std::fs::create_dir_all(&id_dir).expect("identity dir");
        std::fs::write(id_dir.join(USER_KEY_FILE), b"not a key at all").expect("write");
        assert!(
            Identity::load_or_generate(&data_dir).is_err(),
            "a corrupt key must not be silently treated as absent"
        );
    }

    /// A sealed key round-trips through the file the CLI actually writes.
    #[test]
    fn a_sealed_key_loads_with_the_passphrase() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("user.key");
        let kp = Keypair::generate().expect("generate");
        let sealed = crate::keyfile::seal(&kp.seed_bytes().expect("software key"), b"hunter2")
            .expect("seal");
        crate::keyfile::write_private(&path, &sealed).expect("write");

        let opened = Keypair::unlock_and_remember(&path, b"hunter2");
        // `unlock_and_remember` needs $XDG_RUNTIME_DIR to keep the unlock.
        // Under a bare test runner there may be none, and the *unsealing*
        // is the part under test — so assert on whichever half ran.
        match opened {
            Ok(k) => assert_eq!(k.user_id(), kp.user_id()),
            Err(e) => assert!(
                e.to_string().contains("XDG_RUNTIME_DIR"),
                "unexpected failure: {e}"
            ),
        }

        let crate::keyfile::Stored::Sealed(s) = crate::keyfile::Stored::read(&path).expect("read")
        else {
            panic!("expected a sealed key on disk");
        };
        assert_eq!(
            s.open(b"hunter2").expect("open"),
            *kp.seed_bytes().expect("software key")
        );
    }

    /// **The fail-closed property.** A sealed key with no unlock must stop
    /// the resolver dead. The bug this prevents is subtle and bad: falling
    /// through to the next candidate mints a *second* identity, signs with
    /// it, and gets an admin refused by name — telling them they are not a
    /// member of a mesh they run, while their real key sits unread.
    #[test]
    fn a_locked_key_refuses_rather_than_minting_a_second_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("node");
        let id_dir = Identity::identity_dir(&data_dir);
        std::fs::create_dir_all(&id_dir).expect("identity dir");

        let real = Keypair::generate().expect("generate");
        let sealed =
            crate::keyfile::seal(&real.seed_bytes().expect("software key"), b"pass").expect("seal");
        crate::keyfile::write_private(&id_dir.join(USER_KEY_FILE), &sealed).expect("write");

        // A fallback that *would* succeed, so the test proves the resolver
        // stopped on purpose rather than for lack of anywhere else to go.
        let fallback = tmp.path().join("home/.meshware/identity/user.key");
        let err = load_cli_user_from(&data_dir, None, std::slice::from_ref(&fallback))
            .expect_err("a locked key must not resolve");
        assert!(
            matches!(err, CoreError::IdentityLocked(_)),
            "expected IdentityLocked, got: {err}"
        );
        assert!(
            !fallback.exists(),
            "the resolver minted a second identity at {} instead of stopping",
            fallback.display()
        );
    }

    // The other half of this property — that an *unsealed* key which cannot
    // be read still falls through, because that is the unenrolled-person
    // path this resolver exists for — is held by
    // `an_unreadable_node_home_falls_back_to_this_persons_own_key` below.
    // The two are a pair: one asserts the stop, one asserts the fallthrough.

    #[test]
    fn generate_sign_verify() {
        let kp = Keypair::generate().expect("generate");
        let msg = b"meshware";
        let sig = kp.sign(msg);
        assert!(verify_signature(&kp.public_bytes(), msg, &sig));
        assert!(!verify_signature(&kp.public_bytes(), b"tampered", &sig));
    }

    #[test]
    fn persist_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity/node.key");
        let (a, created_a) = Keypair::load_or_generate(&path).expect("first");
        assert!(created_a);
        let (b, created_b) = Keypair::load_or_generate(&path).expect("second");
        assert!(!created_b);
        assert_eq!(a.node_id(), b.node_id());
    }

    /// The regression M?: an account that cannot read the daemon's home
    /// used to get `io error: Permission denied (os error 13)` and no way
    /// forward. Not being able to read another account's private key is
    /// the *normal* state for an unenrolled person, so it must produce a
    /// usable identity of their own, not a stop.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_node_home_falls_back_to_this_persons_own_key() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let node_home = tmp.path().join("var-lib-meshware");
        let id_dir = node_home.join("identity");
        std::fs::create_dir_all(&id_dir).expect("identity dir");
        std::fs::write(id_dir.join(USER_KEY_FILE), [7u8; 32]).expect("their key");
        std::fs::set_permissions(&id_dir, std::fs::Permissions::from_mode(0o000)).expect("lock it");

        let mine = tmp.path().join("mine").join("identity").join(USER_KEY_FILE);
        let loaded = load_cli_user_from(&node_home, None, std::slice::from_ref(&mine));

        // Put it back so the temp dir can be cleaned up.
        let _ = std::fs::set_permissions(&id_dir, std::fs::Permissions::from_mode(0o700));

        let kp = loaded.expect("an unreadable node home is not a fatal error");
        assert_ne!(
            kp.public_bytes(),
            Keypair::from_seed([7u8; 32]).public_bytes(),
            "the daemon's key must not have been read"
        );
        assert!(
            mine.exists(),
            "a key of this person's own should have been created"
        );
    }

    #[test]
    fn pkcs8_export_nonempty() {
        let kp = Keypair::generate().expect("generate");
        let der = kp.to_pkcs8_der().expect("pkcs8");
        assert!(der.len() > 32);
    }
}
