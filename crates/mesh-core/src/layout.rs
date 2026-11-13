//! One home, one layout. Every path meshware writes is a function of a
//! single root, and this module is the only place that function lives.
//!
//! Before this, a node's state was spread across three conventions on the
//! same machine: `~/.local/share/meshware` for the op-log and jobs,
//! `~/.config/meshware/meshnode.toml` for the config, `~/.meshware/identity`
//! for the CLI key. Each was individually defensible and the union was
//! indefensible — "where is my data" had three answers, backup instructions
//! had three paths, and `rm -rf` on any one of them left a node that came
//! back up subtly wrong.
//!
//! The home is now `~/.meshware` (or `$MESHWARE_HOME`), and everything is
//! under it:
//!
//! ```text
//! ~/.meshware/
//! ├── meshnode.toml            the config file
//! ├── identity/                node.key, user.key   (0600)
//! ├── mesh/genesis.json        the mesh this node belongs to
//! ├── state/
//! │   ├── oplog.redb           the replicated op-log — the source of truth
//! │   └── history.redb         the local query index over runs and jobs
//! ├── jobs/<job-id>/           per-job work dir, spec, status, logs
//! ├── content/{blobs,manifests}   durable bundles
//! ├── objects/blobs/           ephemeral job output spilled from RAM
//! ├── envs/python-<hash>/      cached interpreter environments
//! ├── git-sources/<name>/      checkouts of registered Git sources
//! ├── archive/                 Parquet run history (Director writes this)
//! ├── logs/                    meshnode.log, when file logging is on
//! └── run/                     socket fallback when XDG_RUNTIME_DIR is unset
//! ```
//!
//! A packaged node is the one exception to "everything under the home": its
//! home is [`SYSTEM_HOME`] and its config is [`SYSTEM_CONFIG`], in `/etc`
//! where configuration management expects it (DOC-5).
//!
//! **The node only needs the first six.** `archive/` is written by Director
//! on a workstation and is never required for a node to run, join, execute
//! or converge — that separation is deliberate and is what keeps `meshnode`
//! a single small binary with no analytics stack inside it.
//!
//! ## Adopting an older install
//!
//! [`Layout::adopt_legacy`] runs once, moves what it finds from the three
//! old locations, and never overwrites: if the new home already has a file,
//! the old one is left alone and reported. A migration that can silently
//! clobber an identity key is worse than no migration.

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Environment variable naming the home directory.
pub const HOME_ENV: &str = "MESHWARE_HOME";

/// The pre-consolidation variable. Still honoured — an operator who set it
/// in a systemd unit should not have their node move on upgrade — but it
/// now names the whole home rather than one directory inside it.
pub const LEGACY_DATA_ENV: &str = "MESHWARE_DATA_DIR";

/// Dropped in the home the first time a legacy install is adopted, so the
/// migration is obviously one-shot and an operator can see what moved.
const ADOPTED_MARKER: &str = ".adopted-legacy";

/// The home a packaged node runs in: what the shipped unit and `meshnode
/// adopt` set `MESHWARE_DATA_DIR` to, and what the `meshnode` service
/// account is created with as its own home directory (DOC-5).
pub const SYSTEM_HOME: &str = "/var/lib/meshware";

/// Where a packaged node's configuration lives (DOC-5).
///
/// In `/etc`, beside every other package's, rather than inside the data
/// directory: that is 0750 and owned by the service account, so a config
/// kept there could only be edited by becoming the account, and no
/// configuration management would think to look for it. Every other home
/// keeps `meshnode.toml` inside itself.
pub const SYSTEM_CONFIG: &str = "/etc/meshware/meshnode.toml";

/// Where meshware keeps everything for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Wrap an explicit root. No filesystem access; see [`Layout::ensure`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    /// Resolve the home directory.
    ///
    /// Precedence, highest first: an explicit path (`--home`/`--data-dir`),
    /// `$MESHWARE_HOME`, `$MESHWARE_DATA_DIR`, then `~/.meshware`.
    pub fn resolve(explicit: Option<&Path>) -> Self {
        if let Some(p) = explicit {
            return Layout::new(p);
        }
        for var in [HOME_ENV, LEGACY_DATA_ENV] {
            if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                return Layout::new(PathBuf::from(v));
            }
        }
        Layout::new(default_home())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Is this the packaged node's home ([`SYSTEM_HOME`])?
    pub fn is_system_home(&self) -> bool {
        same_path(&self.root, Path::new(SYSTEM_HOME))
    }

    // -- the six a node actually needs ------------------------------------

    /// `meshnode.toml`. Read at startup; absent means pure defaults.
    ///
    /// Inside the home — except for the packaged node's home, whose config
    /// is [`SYSTEM_CONFIG`] in `/etc` (DOC-5).
    pub fn config_file(&self) -> PathBuf {
        if self.is_system_home() {
            PathBuf::from(SYSTEM_CONFIG)
        } else {
            self.root.join("meshnode.toml")
        }
    }

    /// `identity/` — the node key and this machine's admin user key.
    pub fn identity_dir(&self) -> PathBuf {
        self.root.join("identity")
    }

    /// `mesh/` — genesis, and anything else that describes *which* mesh.
    pub fn mesh_dir(&self) -> PathBuf {
        self.root.join("mesh")
    }

    /// `state/` — the redb files. Separated from everything else because
    /// this is the directory whose loss is unrecoverable, which makes it
    /// the directory a backup script should be pointed at.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// `state/oplog.redb` — the replicated op-log.
    pub fn oplog(&self) -> PathBuf {
        self.state_dir().join("oplog.redb")
    }

    /// `state/history.redb` — the local query index over runs and jobs.
    /// Derived data: deleting it costs a rebuild, never a fact.
    pub fn history(&self) -> PathBuf {
        self.state_dir().join("history.redb")
    }

    /// `jobs/` — one directory per job.
    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    /// `content/` — durable bundles: blobs and manifests.
    pub fn content_dir(&self) -> PathBuf {
        self.root.join("content")
    }

    /// `objects/` — ephemeral job output spilled out of RAM.
    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    /// `envs/` — content-hash-keyed interpreter environments.
    pub fn envs_dir(&self) -> PathBuf {
        self.root.join("envs")
    }

    /// `git-sources/` — checkouts of registered Git sources.
    pub fn git_sources_dir(&self) -> PathBuf {
        self.root.join("git-sources")
    }

    /// `logs/` — where file logging lands when it is enabled.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `sessions/` — recorded session transcripts, one directory per
    /// session, in the shape a job spool uses (M17 §3.10). Created only
    /// when `[sessions] record` is on: an empty directory in every node
    /// home would advertise a feature nobody turned on.
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// `sinks/` — one directory per sink: its cursor, its spool, and (for
    /// an `ndjson` sink) the rotating file it writes (M31 B1).
    ///
    /// Created on the elected node only, and only for sinks that exist —
    /// like `sessions/`, an empty directory in every node home would
    /// advertise a feature nobody turned on, and this is a feature almost
    /// nobody should turn on.
    pub fn sinks_dir(&self) -> PathBuf {
        self.root.join("sinks")
    }

    /// `run/` — IPC socket fallback when `XDG_RUNTIME_DIR` is unset.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `run/podman.sock` — where this home's account's own podman service
    /// listens, on a system node (`crate::engines`, *Where containers are
    /// made*). Its account's user manager binds it, not the node, which is
    /// why it lives here and not under `/run/user`: the node's unit hides
    /// that directory.
    pub fn engine_socket(&self) -> PathBuf {
        self.run_dir().join("podman.sock")
    }

    // -- the part a node never touches ------------------------------------

    /// `archive/` — long-term Parquet history. Written by Director, read by
    /// anything that speaks Parquet. A node neither creates nor consults it.
    pub fn archive_dir(&self) -> PathBuf {
        self.root.join("archive")
    }

    // -- lifecycle ---------------------------------------------------------

    /// Create the directories a node needs and verify the home is writable.
    ///
    /// `archive/` is deliberately absent: it is created on first ingest by
    /// whoever runs Director, so a plain node never grows an empty directory
    /// implying a feature it does not have.
    pub fn ensure(&self) -> Result<()> {
        self.untangle_identity_file()?;
        for dir in [
            self.root.clone(),
            self.identity_dir(),
            self.mesh_dir(),
            self.state_dir(),
            self.jobs_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| CoreError::Config(format!("{}: {e}", dir.display())))?;
        }
        let probe = self.root.join(".write-probe");
        std::fs::write(&probe, b"ok").map_err(|e| {
            CoreError::Config(format!("{} is not writable: {e}", self.root.display()))
        })?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }

    /// `~/.meshware/identity` used to be the CLI's *key file*, and it is now
    /// the home's identity *directory*. On the machine of anyone who ever ran
    /// the CLI, those are the same path with different types.
    ///
    /// Resolve it in the only order that cannot lose the key: move the file
    /// out of the way first, create the directory, then land the key inside
    /// it as `user.key` — the name the rest of the code already reads. Run
    /// before any `create_dir_all`, which would otherwise fail with a bare
    /// `NotADirectory` and leave an operator staring at their own key file.
    fn untangle_identity_file(&self) -> Result<()> {
        let identity = self.identity_dir();
        if !identity.is_file() {
            return Ok(());
        }
        let parked = self.root.join(".identity-key-migrating");
        std::fs::rename(&identity, &parked)
            .map_err(|e| CoreError::Config(format!("moving {} aside: {e}", identity.display())))?;
        std::fs::create_dir_all(&identity)
            .map_err(|e| CoreError::Config(format!("{}: {e}", identity.display())))?;
        let dest = identity.join("user.key");
        if dest.exists() {
            // Both exist: keep the one already in the new location and leave
            // the old file parked rather than picking a winner silently.
            tracing::warn!(
                parked = %parked.display(),
                kept = %dest.display(),
                "both an old and a new user key were present; kept the new one"
            );
            return Ok(());
        }
        std::fs::rename(&parked, &dest)
            .map_err(|e| CoreError::Config(format!("{}: {e}", dest.display())))?;
        tracing::info!(path = %dest.display(), "adopted the CLI user key into the home");
        Ok(())
    }

    /// Every path this layout defines, for `meshnode config paths` and the
    /// doctor. `(label, path, node_needs_it)`.
    pub fn describe(&self) -> Vec<(&'static str, PathBuf, bool)> {
        vec![
            ("home", self.root.clone(), true),
            ("config", self.config_file(), true),
            ("identity", self.identity_dir(), true),
            ("mesh", self.mesh_dir(), true),
            ("op-log", self.oplog(), true),
            ("history", self.history(), true),
            ("jobs", self.jobs_dir(), true),
            ("content", self.content_dir(), true),
            ("objects", self.objects_dir(), true),
            ("envs", self.envs_dir(), true),
            ("git-sources", self.git_sources_dir(), true),
            ("logs", self.logs_dir(), true),
            ("archive", self.archive_dir(), false),
        ]
    }

    /// Move a pre-consolidation install into this home. Returns one line per
    /// thing moved; an empty vector means there was nothing to adopt.
    ///
    /// Runs at most once per home — the marker file is checked first — and
    /// never overwrites an existing destination.
    pub fn adopt_legacy(&self) -> Result<Vec<String>> {
        // Only the *default* home adopts. An operator who named a home —
        // `--data-dir`, `$MESHWARE_HOME`, a test harness pointing at a temp
        // directory — has said where their data is, and reaching into
        // `~/.local/share/meshware` or `~/.meshware` on their behalf would
        // move somebody else's node into theirs. It nearly did: the first
        // version of this ran for every home, and four parallel tests
        // raced each other for the developer's real CLI key.
        //
        // Nor the packaged node's home, even when it *is* the default — the
        // service account's own home is `SYSTEM_HOME`. Its old config is in
        // `/etc`, which the unit mounts read-only, so a move out of it could
        // only fail, and a failed adoption stops the node starting; doctor
        // names the file instead.
        if self.root != default_home() || self.is_system_home() {
            return Ok(Vec::new());
        }
        self.adopt_from(legacy_moves(&self.root))
    }

    /// [`Layout::adopt_legacy`] with the move list supplied, so the policy —
    /// one-shot, never-overwrite, rename-or-copy — is testable without
    /// reaching into the real user's home directory.
    fn adopt_from(&self, moves: Vec<(PathBuf, PathBuf)>) -> Result<Vec<String>> {
        let marker = self.root.join(ADOPTED_MARKER);
        if marker.exists() {
            return Ok(Vec::new());
        }
        let mut moved = Vec::new();
        for (from, to) in moves {
            if !from.exists() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::Config(format!("{}: {e}", parent.display())))?;
            }
            match rename_or_copy(&from, &to) {
                Ok(()) => moved.push(format!("{} -> {}", from.display(), to.display())),
                // Gone between the check and the move: another process
                // adopted it first, which is the right outcome and not
                // something to fail a node start over.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(CoreError::Config(format!(
                        "adopting {}: {e}; move it by hand or set {HOME_ENV} to the old path",
                        from.display()
                    )));
                }
            }
        }
        if !moved.is_empty() {
            std::fs::create_dir_all(&self.root)
                .map_err(|e| CoreError::Config(format!("{}: {e}", self.root.display())))?;
            let note = format!(
                "adopted at {}\n{}\n",
                mesh_proto::now_micros(),
                moved.join("\n")
            );
            let _ = std::fs::write(&marker, note);
        }
        Ok(moved)
    }

    // -- the node Director used to run -------------------------------------

    /// `mesh/genesis.json` — the file whose presence means "this home is in
    /// a mesh".
    pub fn genesis_file(&self) -> PathBuf {
        self.mesh_dir().join("genesis.json")
    }

    /// `director/` — Director's own files: its window settings, the SSH
    /// `known_hosts` it keeps, its log.
    ///
    /// Until 2026-09 it was also a whole node home. Director embedded a peer
    /// of its own there, with its own identity and op-log (MASTERPLAN D6 as
    /// it then stood), so quitting the app took this machine out of the
    /// mesh, and a `meshnode` typed afterwards found no mesh at all and
    /// founded a second one. Director is now a view onto this machine's one
    /// node — the one in this home — and [`Layout::adopt_director_node`]
    /// moves the old peer here.
    pub fn director_dir(&self) -> PathBuf {
        self.root.join(DIRECTOR_DIR)
    }

    /// Make the node Director used to embed *this* home's node.
    ///
    /// Returns one line per thing moved; empty means there was nothing to
    /// adopt. Runs before anything could found a mesh here, which is the
    /// point: on a machine whose only mesh lives in `director/`, founding a
    /// new one is never what anybody meant.
    ///
    /// **Only when this home has no mesh of its own.** A home that already
    /// has one is a node somebody chose to run, and which of two meshes this
    /// machine should be in is a person's decision, not a migration's —
    /// [`Layout::director_node_left_behind`] says what is there instead.
    ///
    /// **Never while the old node is open.** An older Director still running
    /// its embedded peer holds the op-log's lock. Renaming files out from
    /// under it would leave two processes writing one store, so a held lock
    /// is a refusal naming the fix.
    ///
    /// **Resumable, and the genesis last.** A home is "in a mesh" when its
    /// genesis exists, so the genesis is the final move: an adoption cut off
    /// half-way is simply finished by the next start instead of leaving a
    /// home that thinks it is done. The node's key and its op-log are what
    /// make it *that* node, so either one already being here — beside
    /// Director's — is refused before anything moves, rather than producing
    /// a node that is half one member and half another.
    ///
    /// Director's node settings — its name, content policy and `.mesh`
    /// responder, kept in `director.json` — become this home's
    /// `meshnode.toml` if it has none, so the node comes back under the same
    /// name rather than as a renamed member. Its tags, `no-execute` among
    /// them, need nothing: the op-log records them and a boot keeps them.
    pub fn adopt_director_node(&self) -> Result<Vec<String>> {
        // The packaged node's home belongs to a service account that has
        // never run Director.
        if self.is_system_home() {
            return Ok(Vec::new());
        }
        let old = Layout::new(self.director_dir());
        if !old.genesis_file().exists() || self.genesis_file().exists() {
            return Ok(Vec::new());
        }
        if store_in_use(&old.oplog()) {
            return Err(CoreError::Config(format!(
                "this machine's mesh is still being run by Director, from {}. Quit \
                 Director — or update it: current versions of Director no longer run a \
                 node of their own — and start this again.",
                old.root().display()
            )));
        }

        let old_node_key = old.identity_dir().join(NODE_KEY_FILE);
        let node_key = self.identity_dir().join(NODE_KEY_FILE);
        // Which pieces make a node *that* node. A second copy of either here
        // is not something to merge.
        for (what, from, to) in [
            ("node key", &old_node_key, &node_key),
            ("op-log", &old.oplog(), &self.oplog()),
        ] {
            if from.exists() && to.exists() {
                return Err(CoreError::Config(format!(
                    "{} has no mesh of its own, but it has a {what} ({}) — and so does the \
                     node Director kept in {}. Adopting that node here would mix two \
                     nodes into one. Move one of the two homes aside and start again.",
                    self.root.display(),
                    to.display(),
                    old.root().display()
                )));
            }
        }

        let mut moves = vec![(old_node_key, node_key)];
        for dir in ADOPTED_NODE_DIRS {
            moves.push((old.root.join(dir), self.root.join(dir)));
        }
        // The founder's key, copied down for Director's founding (the old
        // `mirror_user_for_bootstrap`). This home's own `user.key` is the
        // person's key and wins; the copy is set beside it, never deleted,
        // because a key is the one file nobody can make again.
        let old_user_key = old.identity_dir().join(USER_KEY_FILE);
        let user_key = self.identity_dir().join(USER_KEY_FILE);
        // The usual case: the copy is the key it was copied from — byte for
        // byte, or the same identity since protected with a passphrase
        // (`identity protect` seals the person's key and never knew about
        // this copy). Keeping it would leave an unprotected second copy of
        // a secret somebody chose to protect, and removing it loses nothing.
        if user_key.exists() && same_key(&old_user_key, &user_key) {
            std::fs::remove_file(&old_user_key)
                .map_err(|e| CoreError::Config(format!("{}: {e}", old_user_key.display())))?;
        }
        if user_key.exists() {
            moves.push((
                old_user_key,
                self.identity_dir().join("user.key.from-director"),
            ));
        } else {
            moves.push((old_user_key, user_key));
        }
        // Last, so that until everything else is across this home still
        // reads as not-yet-adopted (see above).
        moves.push((old.mesh_dir(), self.mesh_dir()));

        let mut moved = Vec::new();
        for (from, to) in moves {
            if !from.exists() {
                continue;
            }
            // `ensure` makes `identity/`, `mesh/`, `state/` and `jobs/` on
            // every start, so a failed `meshnode up` leaves them here empty.
            // Empty is not a conflict.
            if to.is_dir() && dir_is_empty(&to) {
                std::fs::remove_dir(&to)
                    .map_err(|e| CoreError::Config(format!("{}: {e}", to.display())))?;
            }
            if to.exists() {
                tracing::warn!(
                    from = %from.display(),
                    to = %to.display(),
                    "left in Director's home: this home already has it"
                );
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CoreError::Config(format!("{}: {e}", parent.display())))?;
            }
            match rename_or_copy(&from, &to) {
                Ok(()) => moved.push(format!("{} -> {}", from.display(), to.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(CoreError::Config(format!(
                        "moving {} to {}: {e}",
                        from.display(),
                        to.display()
                    )));
                }
            }
        }
        // An `identity/` left in `director/` would make it look like a node
        // home to anything that goes looking (`ipc::find_node_home`).
        if dir_is_empty(&old.identity_dir()) {
            let _ = std::fs::remove_dir(old.identity_dir());
        }

        let config = self.config_file();
        if !config.exists()
            && let Some(text) = config_from_director_settings(
                &self.director_dir().join(DIRECTOR_SETTINGS),
                &old.root,
            )
        {
            crate::keyfile::write_private(&config, text.as_bytes())
                .map_err(|e| CoreError::Config(format!("{}: {e}", config.display())))?;
            moved.push(format!(
                "{} -> {}",
                self.director_dir().join(DIRECTOR_SETTINGS).display(),
                config.display()
            ));
        }

        if !moved.is_empty() {
            let note = format!(
                "the node Director kept in {} became this home's node at {}\n{}\n",
                old.root.display(),
                mesh_proto::now_micros(),
                moved.join("\n")
            );
            let _ = std::fs::write(self.root.join(ADOPTED_DIRECTOR_MARKER), note);
        }
        Ok(moved)
    }

    /// Director's old node, when it was *not* adopted because this home has
    /// a mesh of its own: a sentence saying what it is and what to do.
    ///
    /// `None` when there is no such node. Worded for both cases, because
    /// they need different things from a person: the same mesh means a
    /// member record that no process runs any more; a different mesh means
    /// this machine is in the wrong one, or in two, and only they know
    /// which.
    pub fn director_node_left_behind(&self) -> Option<String> {
        let old = Layout::new(self.director_dir());
        if !old.genesis_file().exists() {
            return None;
        }
        let theirs = crate::genesis::Genesis::load(old.root()).ok().flatten()?;
        let ours = crate::genesis::Genesis::load(&self.root).ok().flatten()?;
        let (theirs, ours) = (theirs.body, ours.body);
        Some(if theirs.mesh_id == ours.mesh_id {
            format!(
                "{} still holds the node Director used to run in mesh '{}'. This \
                 machine's node is the one in {}, in the same mesh, so that one is no \
                 longer needed: retire its member record with `meshnode nodes revoke \
                 <its name> --yes`, and the directory by hand once you are sure.",
                old.root.display(),
                ours.mesh_name,
                self.root.display()
            )
        } else {
            format!(
                "this machine is in two meshes: '{}' ({}) in {}, and '{}' ({}) in {}, \
                 which Director used to run. Only the first is running now. To run the \
                 other instead, stop this node, move {}'s mesh/, state/ and \
                 identity/node.key aside, and start it again — it will adopt Director's.",
                ours.mesh_name,
                ours.mesh_id,
                self.root.display(),
                theirs.mesh_name,
                theirs.mesh_id,
                old.root.display(),
                self.root.display()
            )
        })
    }
}

/// `<home>/director` — see [`Layout::director_dir`].
pub const DIRECTOR_DIR: &str = "director";

/// Director's own settings file, inside [`DIRECTOR_DIR`].
pub const DIRECTOR_SETTINGS: &str = "director.json";

/// Dropped in the home when Director's old node is adopted into it: a record
/// of what moved, for a person wondering where their files went.
const ADOPTED_DIRECTOR_MARKER: &str = ".adopted-director";

const NODE_KEY_FILE: &str = "node.key";
const USER_KEY_FILE: &str = "user.key";

/// What moves from Director's old home besides the keys and the genesis:
/// everything the node wrote. Director's own files — `director.json`,
/// `known_hosts`, `logs/`, `bench/` — stay where they are.
const ADOPTED_NODE_DIRS: [&str; 9] = [
    "state",
    "content",
    "jobs",
    "objects",
    "envs",
    "git-sources",
    "sessions",
    "sinks",
    "archive",
];

/// Is some process holding this redb store open?
///
/// redb takes an exclusive `flock` for as long as a database is open, and
/// `File::try_lock` asks for the same lock, so this is a direct question to
/// the kernel rather than a guess from a socket or a pid file. The probe's
/// own lock is released when the file is dropped, before anything is moved.
fn store_in_use(store: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store)
    else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Do these two key files hold the same key — as bytes, or as a plain seed
/// and a sealed file that opens to it?
///
/// A sealed file can only be compared while its session unlock is cached;
/// locked, the answer is "not known to be", and the caller keeps both.
fn same_key(copy: &Path, original: &Path) -> bool {
    use crate::keyfile::Stored;
    let (Ok(a), Ok(b)) = (std::fs::read(copy), std::fs::read(original)) else {
        return false;
    };
    if a == b {
        return true;
    }
    match (Stored::parse_bytes(&a), Stored::parse_bytes(&b)) {
        (Ok(Stored::Plain(seed)), Ok(Stored::Sealed(sealed))) => {
            crate::keyfile::cache_get(original)
                .and_then(|kek| sealed.open_with_kek(&kek).ok())
                .is_some_and(|opened| opened == seed)
        }
        _ => false,
    }
}

fn dir_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// The `meshnode.toml` that says what Director's `director.json` said about
/// the node it ran — or `None` when it said nothing worth writing.
///
/// Read as loose JSON rather than as Director's `Settings` type: that type
/// lives in the app, and a migration must read whatever an older app wrote.
/// A field that is missing, malformed or at its default is left out, so
/// the node's own default applies — which is what the embedded node got.
fn config_from_director_settings(settings: &Path, old_home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(settings).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let quoted = |s: &str| toml::Value::String(s.to_string()).to_string();

    let mut top = Vec::new();
    if let Some(name) = json["node_name"].as_str().map(str::trim)
        && !name.is_empty()
    {
        top.push(format!("node_name = {}", quoted(name)));
    }
    // **Not the tags.** The op-log already has them: this node announced
    // them on every boot, and a boot keeps every recorded tag that is not
    // runtime-managed (`capacity::refreshed_tags`) — so `no-execute` comes
    // back without being written anywhere. Written here it could never go
    // again: a config tag is re-announced on every start, and switching
    // "contribute" on would last until the next restart.

    let mut retention = Vec::new();
    if let Some(policy) = json["content_policy"].as_str() {
        retention.push(format!("content = {}", quoted(policy)));
    }
    if let Some(mb) = json["content_max_mb"].as_u64().filter(|mb| *mb > 0) {
        retention.push(format!("content_max_mb = {mb}"));
    }

    let mut network = Vec::new();
    match json["dns_bind"].as_str().map(str::trim) {
        Some("off") => network.push("dns = false".to_string()),
        Some(addr) if !addr.is_empty() => network.push(format!("dns_bind = {}", quoted(addr))),
        _ => {}
    }

    if top.is_empty() && retention.is_empty() && network.is_empty() {
        return None;
    }
    let mut out = format!(
        "# This machine's node used to run inside Director, from {}.\n\
         # Director is now a view onto this node, and these are the settings it\n\
         # kept for it in director.json. Edit them here from now on.\n",
        old_home.display()
    );
    for line in top {
        out.push_str(&line);
        out.push('\n');
    }
    for (section, lines) in [("retention", retention), ("network", network)] {
        if !lines.is_empty() {
            out.push_str(&format!("\n[{section}]\n"));
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// Tighten this process's file-creation mask to `0o077` (S-L1).
///
/// Everything under the home is private by nature — the op-log holds users,
/// grants, the audit trail and sealed secret records; `jobs/` holds job
/// output; `identity/` holds keys — and almost all of it was created with
/// `create_dir_all`, which means `0777 & ~umask`, which on an ordinary
/// account means `0755`. The packaged install was covered (`UMask=0077` in
/// the systemd unit, `chmod 0750` in `postinst`); a tarball install, a user
/// install and Director's own home were not, so any local user could read
/// the replicated state and the logs.
///
/// One line at the top of `main` rather than a mode argument at every
/// `create_dir_all`, because the ones that get forgotten are the ones added
/// later. Explicit modes stay where they carry a stronger promise than the
/// mask does — a key file is `0600` and a secret file `0400` whatever the
/// mask says.
///
/// It is inherited by job processes, which is the same thing that already
/// happens on a packaged install; the two now agree instead of differing by
/// how the node was installed.
///
/// Returns the previous mask, which nothing needs and which is returned
/// because `umask` does.
#[cfg(unix)]
pub fn tighten_umask() -> u32 {
    use rustix::fs::Mode;
    // 0o077: no group, no other, on anything this process creates.
    let previous = rustix::process::umask(Mode::from_bits_truncate(0o077));
    previous.bits()
}

/// `~/.meshware`. Not `dirs::data_dir()`: the point is a path an operator can
/// type, say out loud, and `tar` without looking it up, and on Linux that
/// rules out `~/.local/share`.
///
/// Except for the `meshnode` service account, whose home directory *is* the
/// packaged node's home ([`SYSTEM_HOME`]): for it, that is the answer, so
/// `sudo -u meshnode meshnode doctor` finds the node the unit runs rather
/// than an empty `/var/lib/meshware/.meshware` (DOC-5).
pub fn default_home() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if same_path(&home, Path::new(SYSTEM_HOME)) {
        return PathBuf::from(SYSTEM_HOME);
    }
    home.join(".meshware")
}

/// The same directory, however it was spelled: canonical where both exist,
/// otherwise compared as written without a trailing slash.
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.components().eq(b.components()),
    }
}

/// The old locations, paired with where each one now lives.
fn legacy_moves(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut moves = Vec::new();
    let home = dirs::home_dir();
    if let Some(old) = dirs::data_dir().map(|d| d.join("meshware"))
        && old != *root
    {
        for name in [
            "identity",
            "mesh",
            "state",
            "jobs",
            "content",
            "objects",
            "envs",
            "git-sources",
            "logs",
        ] {
            moves.push((old.join(name), root.join(name)));
        }
    }
    if let Some(old) = dirs::config_dir().map(|d| d.join("meshware").join("meshnode.toml")) {
        moves.push((old, root.join("meshnode.toml")));
    }
    // The CLI key that used to live at `~/.meshware/identity` is *not*
    // listed here: when the home is the default it is the same path as the
    // identity directory, and `Layout::untangle_identity_file` is the only
    // ordering that survives that. A non-default home still picks it up.
    if let Some(old) = home.map(|h| h.join(".meshware").join("identity"))
        && old.is_file()
        && old != root.join("identity")
    {
        moves.push((old, root.join("identity").join("user.key")));
    }
    moves
}

/// `rename` where possible, recursive copy where the old and new homes are on
/// different filesystems — which is the normal case when `$MESHWARE_HOME`
/// points at a data volume.
fn rename_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_dir(from, to)?;
        std::fs::remove_dir_all(from)
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_lives_under_the_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path());
        for (label, path, _) in layout.describe() {
            assert!(
                path.starts_with(dir.path()),
                "{label} escaped the home: {}",
                path.display()
            );
        }
    }

    #[test]
    fn ensure_makes_a_usable_home_and_no_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("nested").join("home"));
        layout.ensure().expect("ensure");
        assert!(layout.state_dir().is_dir());
        assert!(layout.identity_dir().is_dir());
        assert!(layout.jobs_dir().is_dir());
        // A node must not grow an analytics directory it never writes.
        assert!(!layout.archive_dir().exists());
    }

    #[test]
    fn explicit_root_beats_the_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::resolve(Some(dir.path()));
        assert_eq!(layout.root(), dir.path());
    }

    #[test]
    fn a_packaged_node_keeps_its_config_in_etc() {
        // DOC-5: the packaged home's config is where configuration
        // management looks for it, not inside a 0750 data directory.
        assert_eq!(
            Layout::new(SYSTEM_HOME).config_file(),
            PathBuf::from(SYSTEM_CONFIG)
        );
        assert!(
            Layout::new("/var/lib/meshware/").is_system_home(),
            "a trailing slash is the same home"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            Layout::new(dir.path()).config_file(),
            dir.path().join("meshnode.toml"),
            "every other home keeps its config inside itself"
        );
    }

    #[test]
    fn the_packaged_home_never_adopts() {
        // Its old config is in /etc, which the unit mounts read-only: a
        // move could only fail, and a failed adoption stops the node.
        assert!(
            Layout::new(SYSTEM_HOME)
                .adopt_legacy()
                .expect("adopt")
                .is_empty()
        );
    }

    #[test]
    fn a_named_home_never_adopts_from_the_default_locations() {
        // The guard that stops a node with an explicit home from vacuuming
        // up another node's — or a developer's — data.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("elsewhere"));
        layout.ensure().expect("ensure");
        assert!(layout.adopt_legacy().expect("adopt").is_empty());
    }

    #[test]
    fn adoption_moves_an_old_install_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old");
        std::fs::create_dir_all(old.join("identity")).expect("mkdir");
        std::fs::write(old.join("identity").join("node.key"), b"nodekey").expect("write");
        std::fs::write(old.join("oplog.redb"), b"oplog").expect("write");

        let layout = Layout::new(dir.path().join("home"));
        layout.ensure().expect("ensure");
        let moves = vec![
            (old.join("identity"), layout.identity_dir()),
            (old.join("oplog.redb"), layout.oplog()),
        ];

        // `ensure` already created an empty `identity/`, and an existing
        // destination is skipped — so the key lands via the second pass
        // only because the file itself does not exist yet.
        let moved = layout.adopt_from(moves.clone()).expect("adopt");
        assert_eq!(moved.len(), 1, "moved: {moved:?}");
        assert_eq!(std::fs::read(layout.oplog()).expect("read"), b"oplog");

        // One-shot: the marker stops a second pass from touching anything.
        std::fs::write(old.join("oplog.redb"), b"newer").expect("rewrite");
        let again = layout.adopt_from(moves).expect("adopt again");
        assert!(again.is_empty());
        assert_eq!(std::fs::read(layout.oplog()).expect("read"), b"oplog");
    }

    #[test]
    fn adoption_never_overwrites_a_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old");
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::write(old.join("meshnode.toml"), b"old config").expect("write");

        let layout = Layout::new(dir.path().join("home"));
        layout.ensure().expect("ensure");
        std::fs::write(layout.config_file(), b"live config").expect("write");

        let moved = layout
            .adopt_from(vec![(old.join("meshnode.toml"), layout.config_file())])
            .expect("adopt");
        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read(layout.config_file()).expect("read"),
            b"live config"
        );
        assert!(old.join("meshnode.toml").exists(), "source left in place");
    }

    #[test]
    fn the_old_cli_key_file_becomes_identity_user_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("home");
        std::fs::create_dir_all(&root).expect("mkdir");
        // Exactly what an upgrading machine looks like: `identity` is a file.
        std::fs::write(root.join("identity"), b"seedbytes").expect("write");

        let layout = Layout::new(&root);
        layout.ensure().expect("ensure over a key file");

        assert!(layout.identity_dir().is_dir());
        assert_eq!(
            std::fs::read(layout.identity_dir().join("user.key")).expect("read"),
            b"seedbytes"
        );
        assert!(!root.join(".identity-key-migrating").exists());
    }

    #[test]
    fn a_cross_filesystem_move_copies_then_removes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("old");
        std::fs::create_dir_all(from.join("sub")).expect("mkdir");
        std::fs::write(from.join("a.txt"), b"alpha").expect("write");
        std::fs::write(from.join("sub").join("b.txt"), b"beta").expect("write");

        let to = dir.path().join("new");
        rename_or_copy(&from, &to).expect("move");

        assert!(!from.exists());
        assert_eq!(std::fs::read(to.join("a.txt")).expect("read"), b"alpha");
        assert_eq!(
            std::fs::read(to.join("sub").join("b.txt")).expect("read"),
            b"beta"
        );
    }

    // -- Director's old node -------------------------------------------------

    /// A home the way an older Director left it: the person's own key at
    /// the top, and a whole node — key, genesis, op-log, a job, the
    /// founder-key copy and Director's settings — under `director/`.
    fn home_with_director_node(root: &Path) -> Layout {
        let layout = Layout::new(root);
        let old = Layout::new(layout.director_dir());
        for dir in [
            layout.identity_dir(),
            old.identity_dir(),
            old.mesh_dir(),
            old.state_dir(),
            old.jobs_dir().join("j1"),
            old.logs_dir(),
        ] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        std::fs::write(layout.identity_dir().join("user.key"), b"person").expect("write");
        std::fs::write(old.identity_dir().join("node.key"), b"node").expect("write");
        std::fs::write(old.identity_dir().join("user.key"), b"mirror").expect("write");
        std::fs::write(old.genesis_file(), b"{}").expect("write");
        std::fs::write(old.oplog(), b"oplog").expect("write");
        std::fs::write(old.jobs_dir().join("j1").join("spec"), b"job").expect("write");
        std::fs::write(old.logs_dir().join("director.log"), b"log").expect("write");
        std::fs::write(
            layout.director_dir().join(DIRECTOR_SETTINGS),
            br#"{"node_name":"NIM","tags":["no-execute","console"],
                "content_policy":"ephemeral","content_max_mb":0,"dns_bind":"off",
                "remembered_users":["pi"]}"#,
        )
        .expect("write");
        layout
    }

    #[test]
    fn directors_node_becomes_the_homes_node() {
        // The bug this exists for: Director quit, `meshnode up` found no
        // genesis in the home, and founded a second mesh beside the real one.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        // What a failed `meshnode up` leaves: empty directories, not a node.
        std::fs::create_dir_all(layout.jobs_dir()).expect("mkdir");
        std::fs::create_dir_all(layout.mesh_dir()).expect("mkdir");

        let moved = layout.adopt_director_node().expect("adopt");
        assert!(!moved.is_empty());

        let read = |p: PathBuf| std::fs::read(p).expect("read");
        assert_eq!(read(layout.identity_dir().join("node.key")), b"node");
        assert_eq!(read(layout.oplog()), b"oplog");
        assert_eq!(read(layout.jobs_dir().join("j1").join("spec")), b"job");
        assert!(layout.genesis_file().exists());
        // The person's key is the person's; the copy is kept, not lost.
        assert_eq!(read(layout.identity_dir().join("user.key")), b"person");
        assert_eq!(
            read(layout.identity_dir().join("user.key.from-director")),
            b"mirror"
        );
        // Director's own files stay Director's, and what is left no longer
        // looks like a node home to anything searching for one.
        let director = layout.director_dir();
        assert!(director.join(DIRECTOR_SETTINGS).exists());
        assert!(director.join("logs").join("director.log").exists());
        assert!(!director.join("identity").exists());
        assert!(!director.join("mesh").exists());

        // The node comes back as the same member, making the same choice.
        let text = std::fs::read_to_string(layout.config_file()).expect("config written");
        let config: crate::config::ConfigFile = toml::from_str(&text).expect("valid config");
        assert_eq!(config.node_name.as_deref(), Some("NIM"));
        // In the op-log already, and removable only if they stay there.
        assert_eq!(config.tags, None);
        assert_eq!(config.retention.content.as_deref(), Some("ephemeral"));
        assert_eq!(config.retention.content_max_mb, None, "0 is the default");
        assert_eq!(config.network.dns, Some(false));

        // Done is done.
        assert!(layout.adopt_director_node().expect("again").is_empty());
    }

    #[test]
    fn a_founder_key_copy_identical_to_the_persons_is_not_kept() {
        // What Director actually wrote: a copy of the person's own key.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        let old = Layout::new(layout.director_dir());
        std::fs::write(old.identity_dir().join("user.key"), b"person").expect("write");

        layout.adopt_director_node().expect("adopt");
        assert_eq!(
            std::fs::read(layout.identity_dir().join("user.key")).expect("read"),
            b"person"
        );
        assert!(
            !layout
                .identity_dir()
                .join("user.key.from-director")
                .exists()
        );
        assert!(!old.identity_dir().exists());
    }

    #[test]
    fn a_founder_key_copy_of_a_key_protected_since_is_not_kept() {
        // The development machine's case: Director copied the person's key
        // down to found the mesh, and the person then sealed their own —
        // leaving an unprotected copy of a protected key in `director/`.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        let old = Layout::new(layout.director_dir());
        let seed = [4u8; 32];
        std::fs::write(old.identity_dir().join("user.key"), seed).expect("write");
        let key = layout.identity_dir().join("user.key");
        let sealed = crate::keyfile::seal(&seed, b"hunter2").expect("seal");
        std::fs::write(&key, &sealed).expect("write");
        let Ok(crate::keyfile::Stored::Sealed(parsed)) = crate::keyfile::Stored::read(&key) else {
            panic!("not sealed");
        };
        // With no `$XDG_RUNTIME_DIR` there is no session to unlock into, and
        // the copy is kept — which is the other half of the rule.
        let unlocked = parsed
            .kek(b"hunter2")
            .is_ok_and(|kek| crate::keyfile::cache_put(&key, &kek).is_ok());

        layout.adopt_director_node().expect("adopt");
        assert_eq!(std::fs::read(&key).expect("read"), sealed, "the seal stays");
        assert_eq!(
            layout
                .identity_dir()
                .join("user.key.from-director")
                .exists(),
            !unlocked,
            "an unprotected copy of an unlocked, matching key is not kept"
        );
        crate::keyfile::cache_clear(&key);
    }

    #[test]
    fn a_home_with_a_mesh_of_its_own_adopts_nothing() {
        // Which mesh a machine belongs to is a person's call.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        std::fs::create_dir_all(layout.mesh_dir()).expect("mkdir");
        std::fs::write(layout.genesis_file(), b"{}").expect("write");

        assert!(layout.adopt_director_node().expect("adopt").is_empty());
        let old = Layout::new(layout.director_dir());
        assert!(old.oplog().exists());
        assert!(old.identity_dir().join("node.key").exists());
        assert!(!layout.config_file().exists());
    }

    #[test]
    fn a_node_director_still_has_open_is_not_moved() {
        // Renaming a store out from under the process writing it would
        // leave two processes writing one op-log.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        let old = Layout::new(layout.director_dir());
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(old.oplog())
            .expect("open");
        held.lock().expect("lock");

        let err = layout.adopt_director_node().expect_err("must refuse");
        assert!(err.to_string().contains("Quit"), "{err}");
        assert!(old.oplog().exists());
        assert!(!layout.identity_dir().join("node.key").exists());

        drop(held);
        assert!(!layout.adopt_director_node().expect("adopt").is_empty());
    }

    #[test]
    fn two_node_keys_are_refused_before_anything_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        std::fs::write(layout.identity_dir().join("node.key"), b"other").expect("write");

        assert!(layout.adopt_director_node().is_err());
        let old = Layout::new(layout.director_dir());
        assert!(old.oplog().exists(), "nothing moved");
        assert!(old.genesis_file().exists(), "nothing moved");
    }

    #[test]
    fn an_adoption_cut_short_is_finished_by_the_next_start() {
        // The genesis goes last, so a home without one is still adoptable
        // however far an earlier attempt got.
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = home_with_director_node(&dir.path().join(".meshware"));
        let old = Layout::new(layout.director_dir());
        std::fs::rename(
            old.identity_dir().join("node.key"),
            layout.identity_dir().join("node.key"),
        )
        .expect("partial move");

        layout.adopt_director_node().expect("adopt");
        assert!(layout.genesis_file().exists());
        assert!(layout.oplog().exists());
    }
}
