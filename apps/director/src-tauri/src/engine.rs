//! Director's connection to this machine's node.
//!
//! **Director is a view onto the mesh, not a member of it** (MASTERPLAN D6,
//! amended 2026-09). It used to embed a node of its own, in `<meshware
//! home>/director`, and join the mesh as a peer. That made the app something
//! the mesh leaned on: quitting the window took this machine out of the
//! mesh, and a `meshnode` typed afterwards found nothing to talk to — and,
//! told to start one, founded a second mesh beside the real one.
//!
//! Now every machine runs one `meshnode`, in its home (`~/.meshware`), and
//! Director is a client of it over the socket the CLI uses, through the same
//! [`ipc::Client`]. Every request still travels in a user-signed envelope,
//! so RBAC and audit apply to both frontends identically — no longer a rule
//! to keep, but a consequence of there being one door. Either frontend can
//! be closed without the other, or the node, noticing.
//!
//! This module finds that node, starts it when it has a mesh but is not
//! running ([`connect`]), and owns the human identity Director signs with.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::identity::Keypair;
use mesh_core::ipc;
use mesh_core::layout::Layout;
use mesh_proto::{IpcRequest, IpcResponse};

/// Director's own files: `<meshware home>/director` — its window settings,
/// the SSH `known_hosts` it keeps, its log, benchmark reports. None of it is
/// the mesh's, and nothing in it is needed to run a node.
///
/// A demo keeps its own under the scratch root, so trying the demo leaves
/// nothing of itself behind (D-L1.2).
pub fn app_dir() -> PathBuf {
    match demo_root() {
        Some(root) => root.join("director"),
        None => Layout::resolve(None).director_dir(),
    }
}

/// The home of the node Director is showing: this machine's — `~/.meshware`,
/// or `$MESHWARE_HOME`, exactly the one a bare `meshnode` resolves — or,
/// while a demo runs, the demo's founder.
pub fn node_home() -> PathBuf {
    match demo_root() {
        Some(root) => root.join(crate::demo::FOUNDER),
        None => Layout::resolve(None).root().to_path_buf(),
    }
}

/// The scratch directory a demo runs in, while one is running (L1 P3).
///
/// One switch rather than a path threaded through a hundred commands:
/// `demo::demo_start` sets it and `demo::demo_stop` clears it, and every
/// view follows — to the demo founder's socket, and to a scratch copy of
/// Director's own files — without knowing a demo is happening.
static DEMO_ROOT: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

pub fn demo_root() -> Option<PathBuf> {
    DEMO_ROOT.read().ok().and_then(|dir| dir.clone())
}

pub fn set_demo_root(dir: Option<PathBuf>) {
    if let Ok(mut slot) = DEMO_ROOT.write() {
        *slot = dir;
    }
}

fn settings_path() -> PathBuf {
    app_dir().join(mesh_core::layout::DIRECTOR_SETTINGS)
}

/// Director's own settings: what this window remembers, never how the node
/// runs.
///
/// The node's name, tags, content policy and `.mesh` responder used to live
/// here too, because Director *was* the config file for the node it
/// embedded. They are the node's now — in its `meshnode.toml` and in the
/// op-log, where `meshnode` reads and changes them too — and an older file
/// that still has them is read without complaint and adopted once
/// (`Layout::adopt_director_node`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// SSH user names with a record in the OS keychain (`credentials.rs`).
    /// Names only — the secrets live in the keychain, never here.
    #[serde(default)]
    pub remembered_users: Vec<String>,
    /// The folder each script bundle was added from, newest last (SEC-14).
    ///
    /// Kept here, on the Rust side, because writing a `.definitions` file
    /// back needs the folder, and the window naming a folder to write into
    /// is the arbitrary-write the picker tokens exist to close. The window
    /// names a bundle; this says where it came from.
    #[serde(default)]
    pub source_folders: Vec<(String, std::path::PathBuf)>,
    /// The build output folder Updates publishes from (SEC-14), for the
    /// same reason.
    #[serde(default)]
    pub release_build_dir: Option<std::path::PathBuf>,
}

/// Folders remembered at once; the oldest go first.
const MAX_SOURCE_FOLDERS: usize = 256;

impl Settings {
    /// Remember that `bundle_id` was added from `folder`.
    pub fn remember_source(&mut self, bundle_id: &str, folder: &std::path::Path) {
        self.source_folders.retain(|(b, _)| b != bundle_id);
        self.source_folders
            .push((bundle_id.to_string(), folder.to_path_buf()));
        let excess = self.source_folders.len().saturating_sub(MAX_SOURCE_FOLDERS);
        self.source_folders.drain(..excess);
    }

    /// The folder `bundle_id` was added from, if this Director added it.
    pub fn source_of(&self, bundle_id: &str) -> Option<std::path::PathBuf> {
        self.source_folders
            .iter()
            .rev()
            .find(|(b, _)| b == bundle_id)
            .map(|(_, p)| p.clone())
    }

    pub fn load() -> Option<Settings> {
        let bytes = std::fs::read(settings_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = app_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Merged into what is there rather than written over it: an older
        // file's node settings are what `adopt_director_node` reads, and a
        // save before that has run must not throw them away.
        let mut doc = std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(doc), serde_json::Value::Object(ours)) = (
            doc.as_object_mut(),
            serde_json::to_value(self).map_err(|e| e.to_string())?,
        ) {
            doc.extend(ours);
        }
        std::fs::write(
            settings_path(),
            serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }
}

/// One human identity across CLI and Director: `<home>/identity/user.key`
/// (MASTERPLAN §7.1; OS-keychain storage is a planned refinement). Since
/// Director stopped keeping a node of its own this is also, simply, the key
/// in the node's home — the founder's key when this machine founded the
/// mesh, with no copy of it anywhere else.
pub fn load_user() -> Result<Keypair, String> {
    let path = mesh_core::identity::default_user_identity_path();
    let (user, _) = Keypair::load_or_generate(&path).map_err(|e| e.to_string())?;
    Ok(user)
}

/// This machine's node configuration as `meshnode` resolves it, with what
/// the wizard asked for laid over it — for the steps that prepare a home
/// before its node first starts (join, pair).
pub fn node_config(node_name: Option<&str>, tags: Vec<String>) -> Result<NodeConfig, String> {
    NodeConfig::load(
        None,
        &Overrides {
            data_dir: Some(node_home()),
            node_name: node_name.map(str::to_string),
            tags: (!tags.is_empty()).then_some(tags),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

/// Write the wizard's choices into the node's `meshnode.toml`, where the
/// node — started without flags, as a service — will read them.
pub fn save_node_settings(node_name: &str, mesh_name: Option<&str>) -> Result<(), String> {
    use mesh_core::config::Setting;
    let mut edits = vec![("node_name", Setting::Text(node_name.trim().to_string()))];
    if let Some(name) = mesh_name {
        edits.push(("mesh.name", Setting::Text(name.trim().to_string())));
    }
    mesh_core::config::set_in_file(&Layout::new(node_home()).config_file(), &edits)
        .map_err(|e| e.to_string())
}

/// This machine's node, answering — started first if it has a mesh and is
/// not running. `Err` says why not, in words for the window.
///
/// **Started the way `meshnode up --detach` starts it**: the `systemctl
/// --user` unit when one is installed, otherwise the unit is installed first
/// from `binary` (see `meshnode_binary` in `lib.rs`), which also leaves that
/// `meshnode` at `~/.local/bin` for the terminal. Nothing is started for a
/// home with no mesh unless `founding` says the person asked for one: that
/// is the wizard's decision, never a side effect of opening the app.
///
/// **An older Director's node is adopted first** (`adopt_director_node`),
/// so the mesh this Director used to run inside itself is the one it now
/// shows, run by `meshnode`.
pub async fn connect(binary: Option<PathBuf>, founding: bool) -> Result<Engine, String> {
    let user = load_user()?;
    let home = node_home();
    let socket = mesh_core::config::socket_path_for(&home);
    let engine = Engine::new(ipc::Client::new(socket.clone(), user.clone()), user);
    if demo_root().is_some() {
        // The demo started its founder itself; there is nothing to adopt,
        // install or start here.
        return if answers(&socket).await {
            Ok(engine)
        } else {
            Err("the demo's node is not answering".into())
        };
    }
    // Moving files and asking systemd both block, so neither holds an
    // async worker while it does.
    let prepared = tokio::task::spawn_blocking({
        let home = home.clone();
        move || -> Result<(), String> {
            let layout = Layout::new(&home);
            for moved in layout.adopt_director_node().map_err(|e| e.to_string())? {
                tracing::info!(%moved, "adopted the node Director used to run");
            }
            if let Some(note) = layout.director_node_left_behind() {
                tracing::warn!("{note}");
            }
            Ok(())
        }
    })
    .await
    .map_err(|_| "preparing this machine's node panicked — see Director's log".to_string())?;
    prepared?;
    if answers(&socket).await {
        return Ok(engine);
    }
    if !founding && !Layout::new(&home).genesis_file().exists() {
        return Err("no mesh yet — run the wizard".into());
    }
    // A machine reset out of its last mesh waits to be adopted and refuses
    // to found one by itself (`mesh_core::reset`). The wizard's Create is
    // the person deciding otherwise.
    if founding {
        mesh_core::reset::clear(&Layout::new(&home)).map_err(|e| e.to_string())?;
    }
    let started = tokio::task::spawn_blocking(move || start_node(binary.as_deref(), &home))
        .await
        .map_err(|_| "starting this machine's node panicked — see Director's log".to_string())??;
    wait_until_answering(&socket, &started).await?;
    Ok(engine)
}

/// Start the node for `home`, by whichever of the unit and a plain process
/// fits (see [`connect`]).
fn start_node(binary: Option<&Path>, home: &Path) -> Result<mesh_core::usernode::Started, String> {
    use mesh_core::usernode;
    let missing = || {
        "Director could not find a meshnode to run this machine's node with. Install \
         meshnode, or reinstall Director, which carries one."
            .to_string()
    };
    let service_home = usernode::home().is_some_and(|h| same_dir(&h, home));
    let started = if !service_home {
        // `$MESHWARE_HOME` names a home the user unit does not run.
        usernode::run_detached(binary.ok_or_else(missing)?, home)
    } else if usernode::installed() {
        usernode::start(binary.unwrap_or(Path::new("meshnode")), home)
    } else {
        usernode::install(binary.ok_or_else(missing)?)
    };
    started.map_err(|e| e.to_string())
}

async fn answers(socket: &Path) -> bool {
    ipc::connect(socket).await.is_ok()
}

/// A node that was just started, given a minute to open its socket.
///
/// The unit is `Type=notify`, so `systemctl start` has usually waited for
/// it already; a detached process has no such handshake.
async fn wait_until_answering(
    socket: &Path,
    started: &mesh_core::usernode::Started,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !answers(socket).await {
        if std::time::Instant::now() >= deadline {
            return Err(match started {
                mesh_core::usernode::Started::Service => {
                    "this machine's node was started but is not answering yet — \
                     `journalctl --user -u meshnode` says why"
                        .to_string()
                }
                mesh_core::usernode::Started::Process { log, .. } => format!(
                    "this machine's node was started but is not answering yet — its output is \
                     in {}",
                    log.display()
                ),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// -- the engine's own runtime -------------------------------------------------
//
// **Talking to the node does not happen on the user interface's threads.**
//
// Director used to run the mesh itself, and this split existed so that a
// busy window could not stop a peer gossiping. The mesh has left the
// process, and what stays here is every conversation with it — each
// request, each stream, each terminal. The same hazard remains in smaller
// form: `tauri::async_runtime` is one runtime that every
// `#[tauri::command]` is spawned onto, and a command that blocks its
// worker — `tauri-plugin-dialog`'s `save`/`open` on a desktop whose chooser
// never opened was the one that did — would otherwise stall the replies of
// every view and the output of every open terminal behind it.
//
// So conversations with the node get threads of their own, and the seam is
// this module: a request, a stream, a session all go through [`on_engine`]
// or [`spawn`] and run there. Everything that is Director's alone —
// channel pumps, SSH deploys, the self-updater — stays on Tauri's runtime.
// The isolation is two-way: a wedged UI cannot starve the node's replies,
// and a node that stops answering cannot take the window down with it.
static ENGINE_RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

/// The engine's runtime, built on first use and never dropped.
///
/// **Never dropped** is load-bearing rather than lazy: dropping a Tokio
/// runtime from inside an async context panics, and every caller here is
/// inside Tauri's. A `static` that lives for the process cannot be.
///
/// **Built on first use** means a Director sitting on the wizard costs no
/// threads at all; the first request pays for them.
fn engine_rt() -> &'static tokio::runtime::Runtime {
    ENGINE_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            // Named, because "which threads talk to the node" is the first
            // question anybody asks of a Director stack trace.
            .thread_name("mesh-engine")
            .build()
            .expect("Director needs a Tokio runtime to talk to its node and could not build one")
    })
}

/// Run one future **on the engine's runtime** and await its result here.
///
/// Two things come free with the hop:
///
/// * Anything the future spawns binds to the engine's runtime, not the
///   caller's — a stream's reader, a session's pump.
/// * A panic inside comes back as a `JoinError` and leaves here as an
///   ordinary `Err`. Panicking straight through a `#[tauri::command]`
///   instead means the invoke promise never settles: a button that spins
///   for ever and a log line nobody is looking at. `what` names the work
///   so that message can say which.
pub async fn on_engine<F, T>(what: &str, fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    engine_rt().spawn(fut).await.map_err(|e| {
        if e.is_panic() {
            format!("Director panicked while {what} — see Director's log")
        } else {
            format!("{what} was cancelled before it finished")
        }
    })
}

/// Spawn a detached task **on the engine's runtime**, for work nobody is
/// waiting on.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    engine_rt().spawn(fut)
}

/// This machine's node, as Director reaches it: its socket, and the key
/// every request is signed with.
#[derive(Clone)]
pub struct Engine {
    pub client: ipc::Client,
    pub user: Keypair,
    /// How a key held by an agent is asked to sign (2026-09-18).
    pub key: std::sync::Arc<KeyGate>,
}

/// What every error from a paused signer starts with, so the window can
/// tell "Director stopped asking your security key" apart from any other
/// failure and show the one screen that helps.
pub const KEY_PAUSED: &str = "security-key-paused: ";

/// How Director asks a key held by an agent to sign (2026-09-18).
///
/// Director signs every request, and refreshes a dozen things every couple
/// of seconds. Against a key on this disk that costs nothing. Against a key
/// behind an agent it can cost a person: an agent may put a passphrase or
/// PIN prompt on screen for a signature, and it did — once per request,
/// all at once, and again on the next refresh after every Cancel. Somebody
/// dismissed about thirty of them before it stopped.
///
/// So two rules, both here, because every signature Director makes comes
/// through [`Engine`]:
///
/// - **One signature at a time.** A second request waits for the first
///   rather than raising a second prompt beside it. Waiting happens on the
///   blocking pool, never on an async worker.
/// - **The first refusal stops the asking.** A cancelled prompt, a wrong
///   PIN or an unplugged key pauses signing: every request after it fails
///   at once, without going near the agent, until the person says to try
///   again ([`KeyGate::resume`]). Nothing retries by itself.
#[derive(Default)]
pub struct KeyGate {
    turn: std::sync::Mutex<()>,
    paused: std::sync::Mutex<Option<String>>,
}

impl KeyGate {
    /// Why signing is paused, if it is.
    pub fn paused(&self) -> Option<String> {
        self.paused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn pause(&self, why: &str) {
        let mut slot = self.paused.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            tracing::warn!(%why, "the security key refused a signature; Director stopped asking");
            *slot = Some(why.to_string());
        }
    }

    /// Run `f` as the only signature in flight, and nothing more: for a
    /// test or a connection the person asked for, which must not raise a
    /// second prompt beside one already up, and must not pause anything if
    /// it fails. Blocking.
    pub fn alone<T>(&self, f: impl FnOnce() -> T) -> T {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// Ask the key again — the person said to.
    pub fn resume(&self) {
        *self.paused.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Run `f` as the only signature in flight, unless signing is paused;
    /// pause if it fails. Blocking: call it from the blocking pool.
    pub fn one_at_a_time<T>(&self, f: impl FnOnce() -> mesh_core::Result<T>) -> Result<T, String> {
        if let Some(why) = self.paused() {
            return Err(format!("{KEY_PAUSED}{why}"));
        }
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        // Again: the signature ahead of this one may have failed while
        // this one waited, and asking now would be the prompt after Cancel.
        if let Some(why) = self.paused() {
            return Err(format!("{KEY_PAUSED}{why}"));
        }
        f().map_err(|e| {
            let why = e.to_string();
            self.pause(&why);
            format!("{KEY_PAUSED}{why}")
        })
    }
}

impl Engine {
    pub fn new(client: ipc::Client, user: Keypair) -> Engine {
        Engine {
            client,
            user,
            key: std::sync::Arc::default(),
        }
    }

    /// This engine, signing as `user` from now on — after the key file
    /// changed under the window. A fresh gate: the new key has refused
    /// nothing yet.
    pub fn as_user(&self, user: Keypair) -> Engine {
        Engine::new(
            ipc::Client::new(self.client.socket().to_path_buf(), user.clone()),
            user,
        )
    }

    /// Sign one request: at once for a key on this disk, and for a key
    /// behind an agent through [`KeyGate`], on the blocking pool.
    async fn sign_ipc(&self, req: &IpcRequest) -> Result<mesh_proto::SignedIpc, String> {
        if !self.user.is_hardware() {
            return self.user.sign_request(req).map_err(|e| e.to_string());
        }
        let (user, gate, req) = (self.user.clone(), self.key.clone(), req.clone());
        tokio::task::spawn_blocking(move || gate.one_at_a_time(|| user.sign_request(&req)))
            .await
            .map_err(|e| e.to_string())?
    }

    /// One signed request, one response — Denied/Error surface as `Err`.
    pub async fn ipc(&self, req: IpcRequest) -> Result<IpcResponse, String> {
        match self.request(req).await? {
            IpcResponse::Denied { message } => Err(format!("permission denied: {message}")),
            IpcResponse::Error { message } => Err(message),
            other => Ok(other),
        }
    }

    /// One signed request, and the node's answer as it sent it — `Error`
    /// and `Denied` included, for a caller that reads them (a version
    /// conflict it can retry, say).
    pub async fn request(&self, req: IpcRequest) -> Result<IpcResponse, String> {
        let envelope = self.sign_ipc(&req).await?;
        let client = self.client.clone();
        on_engine("waiting for this machine's node", async move {
            client.request_signed(&req, envelope).await
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Sign with the human's key, whether it is on this disk or on a card
    /// (Y2/Y3).
    ///
    /// Every signature this app produces goes through here rather than
    /// through `Keypair::sign`, so a token that is not plugged in becomes a
    /// sentence in the window instead of a panic in a Tauri command
    /// (landmine 3).
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        if !self.user.is_hardware() {
            return self
                .user
                .try_sign(msg)
                .map(|sig| sig.to_vec())
                .map_err(|e| e.to_string());
        }
        // A record is signed when somebody clicks, not on a timer, so this
        // waits its turn here rather than on the blocking pool — briefly,
        // unless a prompt ahead of it is waiting for the same person.
        self.key
            .one_at_a_time(|| self.user.try_sign(msg))
            .map(|sig| sig.to_vec())
    }

    /// One signed request, streamed responses. The channel closes when the
    /// node has nothing more to say (`ipc::Client::stream`), and dropping it
    /// hangs up — which is how a log follow nobody is watching stops.
    pub async fn ipc_stream(&self, req: IpcRequest) -> Result<mpsc::Receiver<IpcResponse>, String> {
        let envelope = self.sign_ipc(&req).await?;
        let client = self.client.clone();
        on_engine("opening a stream", async move {
            client.stream_signed(envelope).await
        })
        .await?
        .map_err(|e| e.to_string())
    }

    /// Open a session and keep **both** halves (M17 §3.3): the exact path
    /// `meshnode shell` drives, over the same socket.
    pub async fn open_session(
        &self,
        node: &str,
        kind: mesh_proto::SessionKind,
        reason: &str,
    ) -> Result<tokio::net::UnixStream, String> {
        let envelope = self
            .sign_ipc(&ipc::session_request(node, kind, reason))
            .await?;
        let client = self.client.clone();
        on_engine("opening a session", async move {
            client.open_session_signed(envelope).await
        })
        .await?
        .map_err(|e| e.to_string())
    }
}

/// Managed Tauri state.
#[derive(Default)]
pub struct Director {
    pub inner: tokio::sync::Mutex<Inner>,
    /// Held across `start_engine`, which cannot hold `inner` while the node
    /// starts. Without it two calls both see no engine and both start the
    /// machine's node, and the second finds the first holding the op-log.
    pub starting: tokio::sync::Mutex<()>,
    /// The `director://` link waiting to be acted on (L2 P5).
    ///
    /// Outside `inner` deliberately: it is set from the deep-link plugin's
    /// callback and from the single-instance handler, neither of which is
    /// async and neither of which should be able to block on the engine
    /// lock while a job stream holds it.
    pub deep_links: crate::deeplink::Pending,
    /// Files a person picked in a native dialog (S3). Outside `inner` for
    /// the same reason `deep_links` is: it is written from a dialog
    /// callback, which is not async.
    pub picked: crate::picked::Picked,
}

#[derive(Default)]
pub struct Inner {
    pub engine: Option<Engine>,
    /// The demo mesh this Director founded, if it founded one (L1 P3).
    /// Holding the children here is what guarantees they do not outlive
    /// the window: dropping this drops them, and each was started with
    /// `up --supervised`.
    pub demo: Option<crate::demo::DemoRun>,
    /// Wizard pairing (joiner side): the pending human decision.
    pub wizard_decision: Option<oneshot::Sender<bool>>,
    /// Wizard pairing (joiner side): the wait itself, so it can be given up
    /// (DIR-6). `None` once nothing is pairing.
    pub wizard_pairing: Option<WizardPairing>,
    /// The background listener for machines waiting to pair on this
    /// network (`pairing::LanWatch`). Runs while this peer is in a position
    /// to let one in; the frontend reads it, it never polls the network.
    pub lan_watch: Option<mesh_core::pairing::LanWatch>,
    /// Live terminals, by the id the frontend holds (M17 §3.12 P6).
    ///
    /// A terminal is the one surface where the browser has to talk *into* a
    /// stream rather than only read from one, and a Tauri `Channel` is
    /// one-way. So the writing half lives here and `terminal_send` /
    /// `terminal_resize` reach it by id.
    pub terminals: std::collections::HashMap<u64, mpsc::Sender<mesh_proto::SessionFrame>>,
    pub next_terminal: u64,
    /// Deploys and SSH upgrades in flight (`deploy.rs`), by the id the
    /// frontend holds: where a person's code confirmation lands, and how
    /// a run is stopped.
    pub deploys: std::collections::HashMap<u64, DeployHandle>,
    pub next_deploy: u64,
    /// Log follows in flight, by job id (**H-10**).
    ///
    /// A `JobLogs { follow: true }` stream runs until the job ends or the
    /// receiver hangs up, and the frontend had no way to say "stop" — it
    /// simply ignored the frames of the job it was no longer showing. So
    /// clicking through three chatty jobs left three tails flowing from the
    /// engine, two of them being decoded, bridged and thrown away, for as
    /// long as the window stayed open.
    ///
    /// Keyed by job rather than by an opaque handle so that selecting the
    /// same job twice cannot leak the first stream, and so `job_logs_stop`
    /// takes the id the frontend already has.
    pub log_follows: std::collections::HashMap<String, tokio_util::sync::CancellationToken>,
    /// Build lines the person has said yes to running on this computer,
    /// this run of the app (DIR-8, `build_run`).
    pub builds_approved: std::collections::HashSet<String>,
}

/// A wizard pairing in progress (DIR-6): how to stop it, and the task to
/// wait on so that stopping it is finished when the command returns.
#[derive(Debug)]
pub struct WizardPairing {
    pub cancel: tokio_util::sync::CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}

/// The handles one deploy run leaves behind while it works.
#[derive(Debug)]
pub struct DeployHandle {
    /// One pending decision per host waiting on its code.
    pub decisions:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, oneshot::Sender<bool>>>>,
    pub cancel: tokio_util::sync::CancellationToken,
}

impl Director {
    pub async fn engine(&self) -> Result<Engine, String> {
        self.inner
            .lock()
            .await
            .engine
            .clone()
            .ok_or_else(|| "not connected to this machine's node yet".to_string())
    }
}

#[cfg(test)]
mod tests {
    //! **Talking to the node does not happen on the user interface's
    //! threads.**
    //!
    //! Four claims, and the third is the one the split exists for. They are
    //! written against `on_engine`/`spawn` rather than against a live node
    //! on purpose: the property is about *which runtime owns the work*.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// The engine's worker threads are named, so a test can ask where it is.
    fn on_engine_thread() -> bool {
        std::thread::current()
            .name()
            .is_some_and(|n| n.starts_with("mesh-engine"))
    }

    /// A caller's runtime, standing in for Tauri's: `workers` threads and
    /// nothing else, so "every worker is blocked" is a state a test can
    /// actually reach.
    fn caller_runtime(workers: usize) -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .thread_name("test-caller")
            .build()
            .expect("caller runtime")
    }

    #[test]
    fn engine_work_runs_on_the_engines_own_threads() {
        let rt = caller_runtime(2);
        let where_it_ran = rt.block_on(async {
            assert!(!on_engine_thread(), "the caller is not an engine thread");
            super::on_engine("a test", async { on_engine_thread() })
                .await
                .expect("no panic")
        });
        assert!(
            where_it_ran,
            "on_engine ran the future on the caller's runtime"
        );
    }

    /// The claim `Node::start` depends on: a task spawned *inside* engine
    /// work belongs to the engine too. `Node::start` is almost nothing but
    /// spawns — gossip, sync, the scheduler — so if this were false the
    /// whole mesh would still end up on the window's threads however the
    /// outer call was routed.
    #[test]
    fn tasks_spawned_by_engine_work_belong_to_the_engine() {
        let rt = caller_runtime(2);
        let nested = rt.block_on(async {
            super::on_engine("a test", async {
                tokio::spawn(async { on_engine_thread() })
                    .await
                    .expect("nested task")
            })
            .await
            .expect("no panic")
        });
        assert!(
            nested,
            "a task spawned by engine work landed on the caller's runtime"
        );
    }

    /// **The point of the whole thing.**
    ///
    /// Every worker on the caller's runtime is blocked in a way Tokio
    /// cannot see — the exact shape of a `#[tauri::command]` waiting on a
    /// native dialog that will never answer. The mesh keeps running
    /// anyway. Before the split there was one runtime, and this was the
    /// bug: jobs that would not start, and a restart the only cure.
    #[test]
    fn a_wedged_caller_runtime_does_not_stop_the_engine() {
        const WORKERS: usize = 4;
        let rt = caller_runtime(WORKERS);
        let done = Arc::new(AtomicBool::new(false));

        // Occupy every worker with a synchronous block, so the caller's
        // runtime has nothing left to schedule with.
        //
        // **Bounded by the clock, not by a flag this test sets on its way
        // out.** Dropping a Tokio runtime waits for whatever its threads
        // are doing, so a wedge released only by a well-behaved ending
        // turns any failure — a panic, an early return, an assertion in
        // the wrong place — into a test that hangs for ever instead of
        // one that reports. A test written to prove an anti-hang property
        // must be the last thing in the suite that can hang.
        let wedge_until = std::time::Instant::now() + Duration::from_secs(20);
        for _ in 0..WORKERS {
            rt.spawn(async move {
                while std::time::Instant::now() < wedge_until {
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        }
        // Give the blocking tasks time to actually be picked up.
        std::thread::sleep(Duration::from_millis(200));

        // Engine work submitted with the caller wedged, and observed from
        // outside it — awaiting from the caller would be waiting on the
        // very runtime that has no worker left to wake us.
        let flag = Arc::clone(&done);
        super::spawn(async move {
            assert!(on_engine_thread());
            flag.store(true, Ordering::SeqCst);
        });

        // Comfortably inside the wedge: if the engine only gets going once
        // the caller frees up, this deadline passes with `done` unset and
        // the test says so.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            done.load(Ordering::SeqCst),
            "engine work did not run while the caller's runtime was fully blocked"
        );
    }

    /// A panic in engine work comes back as an error.
    ///
    /// Panicking straight through a `#[tauri::command]` never settles the
    /// invoke promise: the button spins for ever and the only clue is a
    /// log line. The hop turns that into a sentence on the screen.
    #[test]
    fn a_panic_in_engine_work_becomes_an_error_rather_than_a_hang() {
        let rt = caller_runtime(2);
        let err = rt.block_on(async {
            super::on_engine::<_, ()>("doing something impossible", async {
                panic!("engine exploded");
            })
            .await
            .expect_err("a panic must not look like success")
        });
        assert!(err.contains("panicked"), "{err}");
        assert!(err.contains("doing something impossible"), "{err}");
        // And the runtime is still usable afterwards — one task died, not
        // the mesh.
        let alive = rt.block_on(async {
            super::on_engine("a test", async { 42 })
                .await
                .expect("no panic")
        });
        assert_eq!(alive, 42);
    }
}

#[cfg(test)]
mod source_folder_tests {
    use super::Settings;
    use std::path::Path;

    #[test]
    fn a_bundle_remembers_the_folder_it_came_from_and_the_list_is_bounded() {
        let mut s = Settings::default();
        s.remember_source("bndl-a", Path::new("/home/you/one"));
        s.remember_source("bndl-a", Path::new("/home/you/two"));
        assert_eq!(
            s.source_of("bndl-a").as_deref(),
            Some(Path::new("/home/you/two"))
        );
        assert_eq!(s.source_of("bndl-b"), None);
        for i in 0..300 {
            s.remember_source(&format!("bndl-{i}"), Path::new("/x"));
        }
        assert_eq!(s.source_folders.len(), super::MAX_SOURCE_FOLDERS);
        assert!(
            s.source_of("bndl-a").is_none(),
            "the oldest should have gone"
        );
    }
}
