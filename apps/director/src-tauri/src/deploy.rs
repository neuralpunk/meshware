//! **Add machines** and **upgrade over SSH**: Director's door onto
//! `mesh_core::deploy`.
//!
//! The person has a list of machines and a way in — a password, a key
//! file, or the agent — and wants them in the mesh. This module turns that
//! into, per machine: connect, look, fetch the right binary, install it,
//! write the service, open the port, pair, wait for the person to confirm
//! the code, start, and watch until the mesh sees the node alive. Every
//! step is an event on a channel, so the screen shows what each machine is
//! doing while the others do theirs.
//!
//! What runs on the far machine is not decided here: it comes from
//! `mesh_core::deploy`, the same text `meshnode adopt` sends. This module
//! decides *how* it gets there (`ssh.rs`), *which binary* (`Resolver`),
//! and *how privilege is delivered* — a sudo password on stdin, ahead of
//! the script, when the probe says one is needed.
//!
//! ## The confirmation is not skipped
//!
//! Both pairing codes are shown, side by side — the one this engine
//! derived and the one the machine printed over SSH — and Director says
//! whether they agree. The person's click confirms an agreement; it cannot
//! override a disagreement, because two independently derived codes
//! disagreeing is the one signal the exchange exists to produce, and a
//! button that dismissed it would throw away the argument. **Confirm all**
//! confirms every card whose codes agree, which is what "a rack in one go"
//! means without meaning "skip the check".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mesh_core::deploy::{self, InstallPlan, Privilege, Probe, Scope, Target};
use mesh_core::membership;
use mesh_proto::{IpcRequest, IpcResponse, Liveness};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::engine::{DeployHandle, Director, Engine};
use crate::ssh;
use crate::ssh::{Auth, HostKeyStatus, Session, SshError};
use crate::view::{
    BinarySourceView, DeployAuthDraft, DeployDraft, DeployEvent, DeployPreviewView, SshUpgradeDraft,
};

/// A probe or a short privileged step.
const QUICK: Duration = Duration::from_secs(45);
/// Sending a binary and installing it: a slow link to a far rack is the
/// normal case.
const STEP: Duration = Duration::from_secs(300);
/// The joiner announcing its port.
const LISTEN: Duration = Duration::from_secs(60);
/// The joiner's own `--wait`, and how long the exchange may take.
const PAIR: Duration = Duration::from_secs(300);
/// How long the person has to click. A pairing code expires at five
/// minutes; this stops just short so the screen says "expired" rather
/// than the protocol saying something less clear.
const CONFIRM: Duration = Duration::from_secs(280);
/// Waiting for the mesh to see the node alive after the service starts.
const VERIFY: Duration = Duration::from_secs(90);
/// Waiting for a node to come back on a new version after an SSH upgrade.
const VERIFY_UPGRADE: Duration = Duration::from_secs(150);

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Credentials for one run, zeroed on drop.
#[derive(Clone)]
struct Creds {
    user: Option<String>,
    auth: Auth,
    sudo_password: Option<Zeroizing<String>>,
}

/// Never the secrets (SEC-23): a `{:?}` in a log line or an error is not a
/// place a sudo password should turn up.
impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Creds")
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field(
                "sudo_password",
                &self.sudo_password.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Creds {
    /// Resolve a draft into credentials, reading anything the form left to
    /// the keychain (S3) for exactly the machines in `hosts` before turning
    /// it into `Creds`.
    ///
    /// Two steps rather than one so `from_draft` stays a pure function the
    /// tests can drive without a keychain: this is the only place the OS
    /// store is read on the deploy path.
    async fn resolve(
        d: DeployAuthDraft,
        hosts: &[String],
        picked: &crate::picked::Picked,
    ) -> Result<Creds, String> {
        let machines = crate::credentials::machines(&d.user, hosts)?;
        let d = crate::credentials::resolve_key_file(d, picked)?;
        Creds::from_draft(crate::credentials::resolve_secrets(d, &machines).await?)
    }

    fn from_draft(d: DeployAuthDraft) -> Result<Creds, String> {
        let user = Some(d.user.trim().to_string()).filter(|u| !u.is_empty());
        let password = d.password.filter(|p| !p.is_empty()).map(Zeroizing::new);
        let auth = match d.method.as_str() {
            "password" => Auth::Password(
                password
                    .clone()
                    .ok_or_else(|| "a password is needed for password login".to_string())?,
            ),
            "key" => Auth::Key {
                path: PathBuf::from(
                    d.key_path
                        .as_deref()
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .ok_or_else(|| "pick the private key file to log in with".to_string())?,
                ),
                passphrase: d.passphrase.filter(|p| !p.is_empty()).map(Zeroizing::new),
            },
            "agent" => Auth::Agent,
            other => return Err(format!("unknown login method {other:?}")),
        };
        // sudo wants *a* password: the one given for it, else the SSH
        // password, which is the same one on nearly every machine.
        let sudo_password = d
            .sudo_password
            .filter(|p| !p.is_empty())
            .map(Zeroizing::new)
            .or(password);
        Ok(Creds {
            user,
            auth,
            sudo_password,
        })
    }
}

/// The path a picker token names, when one was given (SEC-14).
fn picked_path(
    picked: &crate::picked::Picked,
    token: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => picked.require(t).map(Some),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Failure, in the words a card shows
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Fail {
    stage: &'static str,
    message: String,
    hint: Option<String>,
}

fn fail(stage: &'static str, message: impl ToString) -> Fail {
    Fail {
        stage,
        message: message.to_string(),
        hint: None,
    }
}

impl Fail {
    fn hint(mut self, h: impl Into<String>) -> Fail {
        self.hint = Some(h.into());
        self
    }
}

impl From<(&'static str, SshError)> for Fail {
    fn from((stage, e): (&'static str, SshError)) -> Fail {
        let hint = match &e {
            SshError::HostKeyChanged { .. } => {
                Some("Nothing was sent to it. Check the machine before trying again.".into())
            }
            SshError::Auth(_) => Some("Check the credentials and try again.".into()),
            _ => None,
        };
        Fail {
            stage,
            message: e.to_string(),
            hint,
        }
    }
}

type Step<T> = Result<T, Fail>;

// ---------------------------------------------------------------------------
// Which binary
// ---------------------------------------------------------------------------

/// A meshnode binary for one architecture, and where it came from.
#[derive(Debug)]
struct Binary {
    /// Shared, never copied per host — see `Session::run` (**M-22**).
    bytes: std::sync::Arc<[u8]>,
    version: String,
    kind: &'static str,
    detail: String,
}

/// Finds a binary per architecture, once per run.
///
/// In order: a file the person pointed at; the newest of what is at hand
/// — a release published in this mesh, a binary bundled with Director, a
/// `meshnode` installed beside it — and, only when none of those exists,
/// the release channel, verified. The order is the trust order: the mesh's
/// own release was admin-published and key-verified; the channel is a
/// download.
struct Resolver {
    engine: Engine,
    explicit: Option<PathBuf>,
    resource_dir: Option<PathBuf>,
    cache: Mutex<HashMap<String, Arc<Binary>>>,
}

impl Resolver {
    fn new(engine: Engine, app: &AppHandle, explicit: Option<PathBuf>) -> Resolver {
        Resolver {
            engine,
            explicit,
            resource_dir: app.path().resource_dir().ok(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn get(&self, arch: &str) -> Result<Arc<Binary>, String> {
        let arch = deploy::normalise_arch(arch).to_string();
        let mut cache = self.cache.lock().await;
        if let Some(b) = cache.get(&arch) {
            return Ok(b.clone());
        }
        let found = self.resolve(&arch).await?;
        let found = Arc::new(found);
        cache.insert(arch, found.clone());
        Ok(found)
    }

    async fn resolve(&self, arch: &str) -> Result<Binary, String> {
        if let Some(path) = &self.explicit {
            return from_file(path, arch).await;
        }
        let mut best: Option<Binary> = None;
        for candidate in [
            from_mesh(&self.engine, arch).await,
            from_bundled(self.resource_dir.as_deref(), arch).await,
            from_local(arch).await,
        ]
        .into_iter()
        .flatten()
        {
            let newer = best
                .as_ref()
                .is_none_or(|b| version_cmp(&candidate.version, &b.version).is_gt());
            if newer {
                best = Some(candidate);
            }
        }
        if let Some(b) = best {
            return Ok(b);
        }
        match crate::channel::fetch_verified(&self.engine, None, arch).await {
            Ok((bytes, _sig, version)) => Ok(Binary {
                bytes: bytes.into(),
                version: version.clone(),
                kind: "channel",
                detail: format!(
                    "downloaded {version} from the release channel, signature verified"
                ),
            }),
            Err(e) => Err(format!(
                "no meshnode for {arch} is at hand — nothing published in this mesh, nothing \
                 bundled, nothing installed beside Director — and the release channel could not \
                 supply one: {e}"
            )),
        }
    }
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    mesh_proto::version_cmp(a, b)
}

/// The version a `meshnode --version` line reports.
pub(crate) fn version_from_output(out: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(out);
    let first = s.lines().next()?.trim();
    let v = first.split_whitespace().last()?;
    let v = v.trim_start_matches('v');
    v.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| v.to_string())
}

/// `meshnode-v0.6.0-…` → `0.6.0`.
fn version_from_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let rest = name.strip_prefix("meshnode-")?;
    let (tag, _) = rest.split_once('-')?;
    let v = tag.trim_start_matches(['v', 'V']);
    v.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| v.to_string())
}

/// Can this machine run a binary of `arch` to ask it its version?
fn runnable_here(arch: &str) -> bool {
    deploy::normalise_arch(std::env::consts::ARCH) == arch
}

async fn version_of_binary(path: &Path, arch: &str) -> Option<String> {
    if !runnable_here(arch) {
        return None;
    }
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(path).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    version_from_output(&out.stdout)
}

async fn from_file(path: &Path, arch: &str) -> Result<Binary, String> {
    if let Some(named) =
        deploy::arch_of_artifact(&path.file_name().unwrap_or_default().to_string_lossy())
        && named != arch
    {
        return Err(format!(
            "{} is a {named} build, and this machine is {arch}",
            path.display()
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let version = match version_of_binary(path, arch).await {
        Some(v) => v,
        None => version_from_name(path).unwrap_or_else(|| "unknown".into()),
    };
    Ok(Binary {
        bytes: bytes.into(),
        version,
        kind: "file",
        detail: format!("the file you picked, {}", path.display()),
    })
}

/// Read one file out of a bundle, however large.
///
/// Not `BundleFileRead`: that one answers in a single frame, so it is
/// capped at a megabyte — fine for the contracts and scripts it was built
/// for, and roughly a twenty-fifth of a meshnode binary. The streaming
/// door hands back one content chunk per frame and says how many bytes it
/// meant to send, which is the difference between a short file and a cut
/// stream — and the difference matters here, because the bytes are about
/// to be installed on somebody's machine.
async fn bundle_file(
    engine: &Engine,
    bundle_id: mesh_proto::BundleId,
    path: &str,
) -> Result<Vec<u8>, String> {
    let mut rx = engine
        .ipc_stream(IpcRequest::BundleFileFetch {
            bundle_id,
            path: path.to_string(),
        })
        .await?;
    let mut data: Vec<u8> = Vec::new();
    while let Some(resp) = rx.recv().await {
        match resp {
            IpcResponse::BundleFileChunk { data: chunk } => data.extend_from_slice(&chunk),
            IpcResponse::BundleFileDone { size } => {
                if data.len() as u64 != size {
                    return Err(format!(
                        "{path} arrived as {} bytes, but its manifest says {size}",
                        data.len()
                    ));
                }
                return Ok(data);
            }
            IpcResponse::Error { message } => return Err(message),
            IpcResponse::Denied { message } => return Err(format!("permission denied: {message}")),
            _ => {}
        }
    }
    Err(format!(
        "the mesh stopped sending {path} after {} bytes",
        data.len()
    ))
}

async fn from_mesh(engine: &Engine, arch: &str) -> Option<Binary> {
    let IpcResponse::Releases(rows) = engine.ipc(IpcRequest::Releases).await.ok()? else {
        return None;
    };
    let row = rows
        .into_iter()
        .filter(|r| r.arch == arch || (r.arch.is_empty() && arch == std::env::consts::ARCH))
        .max_by(|a, b| version_cmp(&a.version, &b.version))?;
    let data = match bundle_file(engine, row.bundle_id, &row.artifact).await {
        Ok(d) => d,
        Err(e) => {
            // One of several places a binary might come from, so this is a
            // candidate that did not work out rather than a failure — but
            // it is not nothing, and a mesh release that cannot be read is
            // exactly the thing an operator would want named.
            tracing::warn!(version = %row.version, why = %e, "could not read the mesh release");
            return None;
        }
    };
    Some(Binary {
        bytes: data.into(),
        version: row.version.clone(),
        kind: "mesh",
        detail: format!(
            "release {} published in this mesh ({} node{} hold it)",
            row.version,
            row.holders,
            if row.holders == 1 { "" } else { "s" }
        ),
    })
}

/// A binary shipped inside Director: `resources/meshnode/meshnode-<arch>-unknown-linux-musl`,
/// with the version in a `VERSION` file beside it when this machine
/// cannot run it to ask.
async fn from_bundled(resource_dir: Option<&Path>, arch: &str) -> Option<Binary> {
    let dir = crate::localnode::bundle_dir(resource_dir)?;
    let path = dir.join(format!("meshnode-{}", deploy::release_target(arch)));
    if !path.is_file() {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let version = match version_of_binary(&path, arch).await {
        Some(v) => v,
        // No `VERSION` beside it and no way to run it here — a cross-arch
        // binary, or a Director on macOS or Windows. **Refuse rather than
        // guess.** This used to fall back to Director's own
        // `CARGO_PKG_VERSION`, which describes Director and says nothing
        // about this file: a Director at 0.7.2 holding a meshnode 0.7.3-dev.1
        // reported the binary as 0.7.2.
        //
        // That is not cosmetic. This version is what Director publishes to
        // the fleet, and `meshnode upgrade` compares versions — so a wrong
        // one names a release that will not match the binary the nodes then
        // refuse or mis-rank. One fewer binary source is the cheaper failure,
        // and the caller already has three others to fall through to.
        None => tokio::fs::read_to_string(dir.join("VERSION"))
            .await
            .ok()
            .map(|v| v.trim().trim_start_matches('v').to_string())
            .filter(|v| !v.is_empty())?,
    };
    Some(Binary {
        bytes: bytes.into(),
        version,
        kind: "bundled",
        detail: "the meshnode shipped inside this Director".into(),
    })
}

/// A `meshnode` installed on this machine, when it could run on that one.
async fn from_local(arch: &str) -> Option<Binary> {
    if !runnable_here(arch) {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("meshnode"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/meshnode"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/meshnode"));
    candidates.push(PathBuf::from("/usr/bin/meshnode"));
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("meshnode"));
        }
    }
    let mut best: Option<(PathBuf, String)> = None;
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        if let Some(v) = version_of_binary(&path, arch).await {
            let newer = best
                .as_ref()
                .is_none_or(|(_, bv)| version_cmp(&v, bv).is_gt());
            if newer {
                best = Some((path, v));
            }
        }
    }
    let (path, version) = best?;
    let bytes = tokio::fs::read(&path).await.ok()?;
    Some(Binary {
        bytes: bytes.into(),
        version,
        kind: "local",
        detail: format!("the meshnode installed at {}", path.display()),
    })
}

/// What each architecture would get, for the review step — described,
/// not fetched.
async fn describe_sources(
    engine: &Engine,
    app: &AppHandle,
    explicit: Option<&Path>,
) -> Vec<BinarySourceView> {
    let mut out = Vec::new();
    let resource_dir = app.path().resource_dir().ok();
    let releases = match engine.ipc(IpcRequest::Releases).await {
        Ok(IpcResponse::Releases(rows)) => rows,
        _ => Vec::new(),
    };
    let can_verify = !crate::channel::trusted_keys(engine).await.is_empty();
    for arch in ["x86_64", "aarch64"] {
        if let Some(path) = explicit {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let named = deploy::arch_of_artifact(&name);
            if named.is_none_or(|n| n == arch) {
                out.push(BinarySourceView {
                    arch: arch.into(),
                    kind: "file".into(),
                    version: version_from_name(path).unwrap_or_else(|| "?".into()),
                    detail: format!("the file you picked, {}", path.display()),
                });
                continue;
            }
        }
        let mesh = releases
            .iter()
            .filter(|r| r.arch == arch || (r.arch.is_empty() && arch == std::env::consts::ARCH))
            .max_by(|a, b| version_cmp(&a.version, &b.version));
        let bundled = resource_dir
            .as_ref()
            .map(|d| {
                d.join("meshnode")
                    .join(format!("meshnode-{}", deploy::release_target(arch)))
            })
            .filter(|p| p.is_file());
        let local = from_local(arch).await;
        let mut best: Option<(String, &str, String)> = None;
        if let Some(r) = mesh {
            best = Some((
                r.version.clone(),
                "mesh",
                format!("release {} published in this mesh", r.version),
            ));
        }
        if let Some(p) = &bundled {
            let v = version_of_binary(p, arch)
                .await
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
            if best
                .as_ref()
                .is_none_or(|(bv, _, _)| version_cmp(&v, bv).is_gt())
            {
                best = Some((
                    v,
                    "bundled",
                    "the meshnode shipped inside this Director".into(),
                ));
            }
        }
        if let Some(l) = local
            && best
                .as_ref()
                .is_none_or(|(bv, _, _)| version_cmp(&l.version, bv).is_gt())
        {
            best = Some((l.version.clone(), "local", l.detail.clone()));
        }
        out.push(match best {
            Some((version, kind, detail)) => BinarySourceView {
                arch: arch.into(),
                kind: kind.into(),
                version,
                detail,
            },
            None if can_verify => BinarySourceView {
                arch: arch.into(),
                kind: "channel".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                detail: "downloaded from the release channel and verified against a trusted key".into(),
            },
            None => BinarySourceView {
                arch: arch.into(),
                kind: "none".into(),
                version: String::new(),
                detail: "nothing at hand, and a download could not be verified — publish a build under Updates, or pick a file below".into(),
            },
        });
    }
    out
}

// ---------------------------------------------------------------------------
// A run
// ---------------------------------------------------------------------------

struct Run {
    engine: Engine,
    channel: Channel<DeployEvent>,
    creds: Creds,
    tags: Vec<String>,
    install_path: Option<String>,
    fleet_upgrade: bool,
    open_firewall: bool,
    replace_existing: bool,
    /// Upgrade over SSH even where the node already reports the version
    /// being pushed. Off, a machine already on it is left alone, which is
    /// right whenever the version number describes the binary; on, the
    /// bytes go anyway, which is what the operator wants when it does not.
    force: bool,
    resolver: Resolver,
    decisions: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    cancel: CancellationToken,
    /// For the notification when a run finishes while the window is hidden.
    app: AppHandle,
}

impl Run {
    fn emit(&self, ev: DeployEvent) {
        let _ = self.channel.send(ev);
    }

    fn stage(&self, host: &str, stage: &str, detail: impl Into<String>) {
        self.emit(DeployEvent::Stage {
            host: host.to_string(),
            stage: stage.to_string(),
            detail: detail.into(),
        });
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn check_cancel(&self, stage: &'static str) -> Step<()> {
        if self.cancelled() {
            Err(fail(stage, "stopped"))
        } else {
            Ok(())
        }
    }
}

struct Done {
    node_name: String,
    node_id: String,
    version: String,
    service: String,
    note: Option<String>,
}

/// The command that runs an inline POSIX script under `sh`, whatever the
/// account's login shell is.
fn sh_c(script: &str) -> String {
    format!("sh -c {}", deploy::shell_quote(script))
}

/// Run a script from stdin under the privilege the probe reported, with
/// the sudo password ahead of it when sudo will read one.
async fn privileged(
    session: &Session,
    elevate: Privilege,
    creds: &Creds,
    script: String,
    within: Duration,
) -> Result<crate::ssh::Output, SshError> {
    let cmd = elevate.wrap(None, "sh -s");
    let mut stdin = Vec::new();
    if elevate.wants_password()
        && let Some(pw) = &creds.sudo_password
    {
        stdin.extend_from_slice(pw.as_bytes());
        stdin.push(b'\n');
    }
    stdin.extend_from_slice(script.as_bytes());
    session.run(&cmd, Some(stdin.into()), within).await
}

/// Connect, and tell the screen about the host key.
async fn open(run: &Run, host: &str, target: &Target) -> Step<Session> {
    let user = target
        .user
        .clone()
        .or_else(|| run.creds.user.clone())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            fail("connecting", "no user name").hint("Write user@host, or fill in the user field.")
        })?;
    run.stage(
        host,
        "connecting",
        format!(
            "{}@{} · {}",
            user,
            target.label().rsplit('@').next().unwrap_or(""),
            run.creds.auth.label()
        ),
    );
    let session = Session::connect(
        target,
        &user,
        &run.creds.auth,
        Some(host_key_approval(run, host)),
    )
    .await
    .map_err(|e| Fail::from(("connecting", e)))?;
    run.emit(DeployEvent::HostKey {
        host: host.to_string(),
        // As `ssh-keygen -l` would print it: the digest, then the kind.
        fingerprint: format!(
            "{} ({})",
            session.host_key.fingerprint, session.host_key.algorithm
        ),
        status: match session.host_key.status {
            HostKeyStatus::Known => "known",
            HostKeyStatus::New => "new",
            HostKeyStatus::Changed => "changed",
        }
        .into(),
    });
    Ok(session)
}

/// The question a new machine's key gets asked, wired to the same decision
/// channel the pairing code uses (S-M9).
///
/// It runs *inside* the SSH handshake, before `authenticate`, so a decline
/// costs nothing — no password has left this process. The old behaviour was
/// to accept whatever key was offered, send the password, and report the
/// fingerprint afterwards.
///
/// A decision that never comes is a decline: the deploy of forty machines
/// cannot park on one of them for ever, and refusing is the safe end of that
/// trade.
fn host_key_approval(run: &Run, host: &str) -> ssh::HostKeyApproval {
    let channel = run.channel.clone();
    let decisions = run.decisions.clone();
    let cancel = run.cancel.clone();
    let host = host.to_string();
    Arc::new(move |key: ssh::HostKey| {
        let channel = channel.clone();
        let decisions = decisions.clone();
        let cancel = cancel.clone();
        let host = host.clone();
        Box::pin(async move {
            let _ = channel.send(DeployEvent::Stage {
                host: host.clone(),
                stage: "confirming".into(),
                detail: "waiting for you to accept this machine's key".into(),
            });
            let _ = channel.send(DeployEvent::HostKeyPrompt {
                host: host.clone(),
                fingerprint: key.fingerprint.clone(),
                algorithm: key.algorithm.clone(),
            });
            let (tx, decision) = oneshot::channel();
            decisions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(host.clone(), tx);
            let accept = tokio::select! {
                d = decision => d.unwrap_or(false),
                _ = tokio::time::sleep(ssh::HOST_KEY_DECISION) => false,
                _ = cancel.cancelled() => false,
            };
            decisions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&host);
            accept
        })
    })
}

/// One round trip: what the machine is.
async fn inspect(run: &Run, host: &str, session: &Session, install_path: &str) -> Step<Probe> {
    run.stage(host, "inspecting", "reading what the machine is");
    let out = session
        .run(
            &sh_c(&deploy::probe_script(install_path, "/var/lib/meshware")),
            None,
            QUICK,
        )
        .await
        .map_err(|e| Fail::from(("inspecting", e)))?;
    if !out.ok() && out.stdout.trim().is_empty() {
        return Err(fail("inspecting", out.complaint()));
    }
    let probe = deploy::parse_probe(&out.stdout).map_err(|e| fail("inspecting", e))?;
    if !probe.os.eq_ignore_ascii_case("linux") {
        return Err(fail(
            "inspecting",
            format!(
                "{} runs {}, and only Linux machines execute jobs",
                probe.hostname, probe.os
            ),
        )
        .hint("A Mac or Windows machine joins by running Director on it instead."));
    }
    Ok(probe)
}

/// The shape a machine gets, from what the probe said and what the
/// person can deliver.
fn plan_for(run: &Run, probe: &Probe) -> (InstallPlan, Privilege, Option<String>) {
    let priv_ = probe.privilege.unwrap_or(Privilege::None);
    let mut note = None;
    let elevate = match priv_ {
        Privilege::Root => Privilege::Root,
        Privilege::SudoNoPassword => Privilege::SudoNoPassword,
        Privilege::SudoPassword if run.creds.sudo_password.is_some() => Privilege::SudoPassword,
        Privilege::SudoPassword => {
            note = Some(format!(
                "{} needs a password for sudo and none was given, so the node lives in that \
                 account's home and runs as it",
                probe.user
            ));
            Privilege::None
        }
        Privilege::None => {
            note = Some(format!(
                "{} cannot reach root (no sudo), so the node lives in that account's home and \
                 runs as it",
                probe.user
            ));
            Privilege::None
        }
    };
    let mut plan = if elevate.can_elevate() {
        InstallPlan::system(run.install_path.as_deref())
    } else {
        let mut p = InstallPlan::user(&probe.home, &probe.user);
        if let Some(path) = run.install_path.as_deref().filter(|p| !p.trim().is_empty()) {
            p.install_path = path.to_string();
        }
        p
    };
    plan.fleet_upgrade = run.fleet_upgrade;
    plan.open_firewall = run.open_firewall && plan.scope == Scope::System;
    (plan, elevate, note)
}

/// Prove the sudo password works before anything depends on it.
async fn check_sudo(run: &Run, session: &Session, elevate: Privilege, user: &str) -> Step<()> {
    if !elevate.wants_password() {
        return Ok(());
    }
    let pw = run
        .creds
        .sudo_password
        .as_ref()
        .ok_or_else(|| fail("inspecting", "sudo needs a password and none was given"))?;
    let mut stdin = pw.as_bytes().to_vec();
    stdin.push(b'\n');
    let out = session
        .run("sudo -k -S -p '' true", Some(stdin.into()), QUICK)
        .await
        .map_err(|e| Fail::from(("inspecting", e)))?;
    if !out.ok() {
        let err = out.stderr.to_ascii_lowercase();
        let msg = if err.contains("tty") {
            "sudo on this machine insists on a terminal (`requiretty` in sudoers), so it cannot \
             take a password from here"
                .to_string()
        } else {
            format!("sudo refused the password for {user}")
        };
        return Err(fail("inspecting", msg).hint(
            "Give the sudo password under Credentials, or untick nothing and let the node \
             install inside that account's home.",
        ));
    }
    Ok(())
}

/// Send the binary and stage it, unless the right one is already there.
async fn stage_binary(
    run: &Run,
    host: &str,
    session: &Session,
    probe: &Probe,
    plan: &InstallPlan,
    bin: &Binary,
) -> Step<Option<String>> {
    let already = probe.version_number().as_deref() == Some(bin.version.as_str())
        && probe.exe.as_deref() == Some(plan.install_path.as_str());
    if already {
        run.stage(
            host,
            "installing",
            format!("meshnode {} is already there", bin.version),
        );
        return Ok(None);
    }
    run.stage(
        host,
        "installing",
        format!(
            "sending meshnode {} ({:.1} MB, {})",
            bin.version,
            bin.bytes.len() as f64 / 1_048_576.0,
            bin.kind
        ),
    );
    let out = session
        .run(
            &sh_c(&InstallPlan::stage_script()),
            Some(bin.bytes.clone()),
            STEP,
        )
        .await
        .map_err(|e| Fail::from(("installing", e)))?;
    if !out.ok() {
        return Err(fail(
            "installing",
            format!("the binary could not be staged: {}", out.complaint()),
        )
        .hint("Is /tmp or the home directory writable there? Is it the right architecture?"));
    }
    let staged = deploy::kv_value(&out.stdout, "staged").ok_or_else(|| {
        fail(
            "installing",
            "the stage step did not say where it put the binary",
        )
    })?;
    Ok(Some(staged.to_string()))
}

/// The one privileged step.
async fn install(
    run: &Run,
    host: &str,
    session: &Session,
    plan: &InstallPlan,
    elevate: Privilege,
    staged: Option<&str>,
) -> Step<(String, Vec<String>)> {
    run.stage(
        host,
        "configuring",
        match plan.scope {
            Scope::System => "service account, systemd unit, firewall",
            Scope::User => "user service in the home directory",
        },
    );
    let out = privileged(
        session,
        elevate,
        &run.creds,
        plan.install_script(staged),
        STEP,
    )
    .await
    .map_err(|e| Fail::from(("configuring", e)))?;
    if !out.ok() || deploy::kv_value(&out.stdout, "installed") != Some("ok") {
        let msg = out.complaint();
        let hint = if msg.to_ascii_lowercase().contains("permission denied") {
            Some(
                "The account could not write there. Give a sudo password, or pick a path inside its home.",
            )
        } else {
            None
        };
        let mut f = fail("configuring", msg);
        if let Some(h) = hint {
            f = f.hint(h);
        }
        return Err(f);
    }
    let service = deploy::kv_value(&out.stdout, "service")
        .unwrap_or("none")
        .to_string();
    let mut notes = Vec::new();
    match deploy::kv_value(&out.stdout, "firewall") {
        Some("failed") => {
            notes.push("the firewall rule could not be added — open UDP 7777 by hand".to_string());
        }
        Some("firewalld") => notes.push("opened UDP 7777 in firewalld".to_string()),
        Some("ufw") => notes.push("opened UDP 7777 in ufw".to_string()),
        _ => {}
    }
    // Enabling the unit is what makes the node come back after a reboot,
    // and it used to fail silently. A machine that rejoins the mesh
    // perfectly and then disappears at its next power cut is the worst
    // kind of working, so a refusal is said out loud on the card.
    if deploy::kv_value(&out.stdout, "enabled") == Some("no") {
        notes.push(match plan.scope {
            Scope::System => "the service could not be enabled, so it will not start after a \
                              reboot — `systemctl enable meshnode` as root fixes it"
                .to_string(),
            Scope::User => "the service could not be enabled, so it will not start at next login \
                            — `systemctl --user enable meshnode` there fixes it"
                .to_string(),
        });
    }
    Ok((service, notes))
}

/// The pairing exchange, with the person in the loop.
async fn pair(
    run: &Run,
    host: &str,
    session: &Session,
    target: &Target,
    plan: &InstallPlan,
    elevate: Privilege,
) -> Step<(String, String)> {
    run.stage(host, "pairing", "starting the pairing exchange");
    let join_cmd = elevate.wrap(
        plan.run_as(),
        &plan.join_command(&run.tags, PAIR.as_secs(), None),
    );
    let mut joiner = session
        .spawn(&join_cmd)
        .await
        .map_err(|e| Fail::from(("pairing", e)))?;
    if elevate.wants_password()
        && let Some(pw) = &run.creds.sudo_password
    {
        let mut line = pw.as_bytes().to_vec();
        line.push(b'\n');
        joiner
            .write(&line)
            .await
            .map_err(|e| Fail::from(("pairing", e)))?;
    }

    // The joiner announces its port, then its code.
    let (port, announced_name) = match next_frame(&mut joiner, LISTEN).await {
        Ok(JoinerFrame::Listening {
            port, node_name, ..
        }) => (port, node_name),
        Ok(JoinerFrame::Error { message }) => {
            joiner.kill().await;
            return Err(fail("pairing", message));
        }
        Ok(other) => {
            joiner.kill().await;
            return Err(fail(
                "pairing",
                format!("unexpected first frame from the joiner: {other:?}"),
            ));
        }
        Err(e) => {
            let err = joiner.stderr().to_string();
            joiner.kill().await;
            return Err(fail(
                "pairing",
                if err.is_empty() {
                    e.message
                } else {
                    format!("the joiner failed to start: {err}")
                },
            )
            .hint("Is UDP 7777 free on that machine? A meshnode already running there holds it."));
        }
    };
    let addr = deploy::dial_addr(target.dial_host(), port);

    // A single-use invite for provenance, exactly as `invite approve`.
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = run
        .engine
        .ipc(IpcRequest::InviteBootstrap)
        .await
        .map_err(|e| fail("pairing", e))?
    else {
        joiner.kill().await;
        return Err(fail("pairing", "unexpected response to InviteBootstrap"));
    };
    let params = membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags: run.tags.clone(),
        bootstrap: addrs,
    };
    let (token, public) = membership::build_invite(mesh_id, &run.engine.user, &params, node_id)
        .map_err(|e| fail("pairing", e))?;
    run.engine
        .ipc(IpcRequest::InvitePublish { public })
        .await
        .map_err(|e| fail("pairing", e))?;
    let mut rx = run
        .engine
        .ipc_stream(IpcRequest::PairApprove {
            addr: addr.clone(),
            token: token.encode(),
        })
        .await
        .map_err(|e| fail("pairing", e))?;

    run.stage(host, "pairing", format!("dialling {addr}"));
    let theirs = match next_frame(&mut joiner, PAIR).await {
        Ok(JoinerFrame::Code { code, .. }) => code,
        Ok(JoinerFrame::Error { message }) => {
            joiner.kill().await;
            return Err(fail("pairing", message));
        }
        Ok(other) => {
            joiner.kill().await;
            return Err(fail(
                "pairing",
                format!("expected a code from the joiner, got {other:?}"),
            ));
        }
        Err(e) => {
            joiner.kill().await;
            return Err(fail("pairing", e.message).hint(
                "This Director could not reach the machine's pairing port. If it sits behind a \
                 NAT or a firewall this side cannot cross, pair it from a machine that can.",
            ));
        }
    };
    let (session_id, ours, joiner_name) = loop {
        let frame = tokio::time::timeout(PAIR, rx.recv())
            .await
            .map_err(|_| fail("pairing", "this engine never produced a pairing code"))?;
        match frame {
            Some(IpcResponse::PairCode {
                session,
                code,
                joiner_name,
                ..
            }) => break (session, code, joiner_name),
            Some(IpcResponse::Denied { message }) | Some(IpcResponse::Error { message }) => {
                let _ = joiner.write(b"no\n").await;
                joiner.kill().await;
                return Err(fail("pairing", message));
            }
            Some(_) => {}
            None => {
                joiner.kill().await;
                return Err(fail("pairing", "the pairing stream ended early"));
            }
        }
    };
    if joiner_name != announced_name {
        let _ = joiner.write(b"no\n").await;
        let _ = run
            .engine
            .ipc(IpcRequest::PairDecide {
                session: session_id,
                accept: false,
            })
            .await;
        joiner.kill().await;
        return Err(fail(
            "pairing",
            format!(
                "the machine at {addr} calls itself {joiner_name:?}, but the one reached over SSH \
                 said {announced_name:?}. Nothing was joined."
            ),
        ));
    }

    let matched = ours == theirs;
    run.emit(DeployEvent::Code {
        host: host.to_string(),
        ours: ours.clone(),
        theirs: theirs.clone(),
        matched,
        node_name: joiner_name.clone(),
    });
    if !matched {
        // Not a retry. Two independently derived codes disagreeing is the
        // single outcome this exchange exists to detect.
        let _ = joiner.write(b"no\n").await;
        let _ = run
            .engine
            .ipc(IpcRequest::PairDecide {
                session: session_id,
                accept: false,
            })
            .await;
        joiner.kill().await;
        return Err(fail(
            "pairing",
            format!(
                "PAIRING CODES DO NOT MATCH — this Director derived {ours}, the machine derived \
                 {theirs}. Somebody is between them. Nothing was joined."
            ),
        )
        .hint("Do not retry until you know why."));
    }

    // The person's click.
    run.stage(host, "confirming", "waiting for you to confirm the code");
    let (tx, decision) = oneshot::channel();
    run.decisions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(host.to_string(), tx);
    let accept = tokio::select! {
        d = decision => d.unwrap_or(false),
        _ = tokio::time::sleep(CONFIRM) => false,
        _ = run.cancel.cancelled() => false,
    };
    run.decisions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(host);
    let _ = joiner.write(if accept { b"yes\n" } else { b"no\n" }).await;
    let _ = run
        .engine
        .ipc(IpcRequest::PairDecide {
            session: session_id,
            accept,
        })
        .await;
    if !accept {
        joiner.kill().await;
        return Err(fail(
            "confirming",
            if run.cancelled() {
                "stopped before the code was confirmed"
            } else {
                "the code was not confirmed in time — nothing was joined"
            },
        ));
    }

    run.stage(host, "joining", "membership landing");
    let done = loop {
        let frame = tokio::time::timeout(PAIR, rx.recv())
            .await
            .map_err(|_| fail("joining", "pairing stalled after confirmation"))?;
        match frame {
            Some(IpcResponse::PairDone { node_id, node_name }) => {
                break (node_id.to_display_string(), node_name);
            }
            Some(IpcResponse::Denied { message }) | Some(IpcResponse::Error { message }) => {
                joiner.kill().await;
                return Err(fail("joining", message));
            }
            Some(_) => {}
            None => {
                joiner.kill().await;
                return Err(fail(
                    "joining",
                    "the pairing stream ended before the membership landed",
                ));
            }
        }
    };
    // Let the joiner write its identity and exit before the service starts
    // on the same data directory.
    let _ = joiner.wait(Duration::from_secs(30)).await;
    Ok(done)
}

/// One NDJSON frame from `meshnode join --pair --json`.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JoinerFrame {
    Listening {
        port: u16,
        #[allow(dead_code)]
        addr: String,
        node_name: String,
    },
    Code {
        code: String,
        #[allow(dead_code)]
        words: Option<String>,
        #[allow(dead_code)]
        mesh_name: String,
    },
    Joined {
        #[allow(dead_code)]
        mesh_name: String,
    },
    Error {
        message: String,
    },
}

/// The next frame the joiner prints, skipping a shell profile's banner.
async fn next_frame(joiner: &mut crate::ssh::Proc, within: Duration) -> Step<JoinerFrame> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(fail("pairing", "the joiner went quiet"));
        }
        let line = joiner
            .next_line(left)
            .await
            .map_err(|e| fail("pairing", e))?;
        let Some(line) = line else {
            let err = joiner.stderr().to_string();
            return Err(fail(
                "pairing",
                if err.is_empty() {
                    "the joiner exited before pairing".to_string()
                } else {
                    format!("the joiner exited: {err}")
                },
            ));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(frame) = serde_json::from_str::<JoinerFrame>(trimmed) {
            return Ok(frame);
        }
    }
}

/// Start the service and wait until the mesh sees the node alive.
async fn start_and_verify(
    run: &Run,
    host: &str,
    session: &Session,
    plan: &InstallPlan,
    elevate: Privilege,
    node_id: &str,
    node_name: &str,
) -> Step<(String, Option<String>, String)> {
    run.stage(host, "starting", "starting the service");
    let out = privileged(session, elevate, &run.creds, plan.start_script(), QUICK)
        .await
        .map_err(|e| Fail::from(("starting", e)))?;
    if !out.ok() {
        // systemd's own words, plus the one cause it describes as
        // something else entirely.
        let f = fail("starting", out.complaint());
        return Err(match deploy::start_failure_hint(&out.stdout) {
            Some(hint) => f.hint(hint),
            None => f,
        });
    }
    let service = deploy::kv_value(&out.stdout, "started")
        .unwrap_or("none")
        .to_string();
    let linger = match deploy::kv_value(&out.stdout, "linger") {
        Some("off") => Some(
            "lingering could not be enabled, so the node stops when that account logs out — \
             `loginctl enable-linger` as root fixes it"
                .to_string(),
        ),
        _ => None,
    };

    run.stage(
        host,
        "verifying",
        format!("waiting for {node_name} to report in"),
    );
    let deadline = tokio::time::Instant::now() + VERIFY;
    let mut version = String::new();
    let mut seen = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(IpcResponse::Nodes(rows)) = run.engine.ipc(IpcRequest::Nodes).await
            && let Some(row) = rows
                .iter()
                .find(|r| r.node_id.to_display_string() == node_id)
            && matches!(row.liveness, Liveness::Alive)
        {
            seen = true;
            version = row.version.clone().unwrap_or_default();
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let note = if seen {
        linger
    } else {
        Some(match linger {
            Some(l) => format!("joined, but not seen alive yet — {l}"),
            None => format!(
                "joined, but not seen alive within {}s — it usually appears shortly; if not, \
                 check `systemctl status meshnode` there",
                VERIFY.as_secs()
            ),
        })
    };
    Ok((service, note, version))
}

/// Everything, for one machine.
async fn deploy_host(run: Arc<Run>, raw: String) -> Step<Done> {
    let host = raw.as_str();
    let target = Target::parse(&raw).map_err(|e| fail("connecting", e))?;
    run.check_cancel("connecting")?;
    let session = open(&run, host, &target).await?;
    let result = deploy_host_inner(&run, host, &target, &session).await;
    session.close().await;
    result
}

async fn deploy_host_inner(
    run: &Run,
    host: &str,
    target: &Target,
    session: &Session,
) -> Step<Done> {
    let probe_path = run
        .install_path
        .clone()
        .unwrap_or_else(|| "/usr/local/bin/meshnode".into());
    let probe = inspect(run, host, session, &probe_path).await?;
    if probe.existing_mesh.is_some() && !run.replace_existing {
        return Err(fail(
            "inspecting",
            format!(
                "{} already belongs to a mesh{}",
                probe.hostname,
                probe
                    .version
                    .as_deref()
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default()
            ),
        )
        .hint(
            "Pairing it again would give one machine two identities. Tick \"Replace an existing \
             install\" to move its old identity aside and pair it fresh, or use Updates to \
             change its version.",
        ));
    }
    let (plan, elevate, mut note) = plan_for(run, &probe);
    // Everything in this plan is about to be interpolated into a systemd
    // unit and a root-run script (S-M5). The install path is a form field;
    // the data and config directories and the account name come from that
    // or from what this machine said about itself over SSH.
    plan.validate().map_err(|e| fail("preparing", e))?;
    check_sudo(run, session, elevate, &probe.user).await?;
    run.emit(DeployEvent::Machine {
        host: host.to_string(),
        hostname: probe.hostname.clone(),
        distro: probe.distro.clone(),
        arch: probe.arch.clone(),
        scope: match plan.scope {
            Scope::System => "system",
            Scope::User => "user",
        }
        .into(),
        note: note.clone(),
    });

    run.check_cancel("preparing")?;
    run.stage(
        host,
        "preparing",
        format!("finding a meshnode for {}", probe.arch),
    );
    let bin =
        run.resolver.get(&probe.arch).await.map_err(|e| {
            fail("preparing", e).hint("Publish a build under Updates, or pick a file.")
        })?;
    run.stage(
        host,
        "preparing",
        format!("meshnode {} · {}", bin.version, bin.detail),
    );

    if run.replace_existing && probe.existing_mesh.is_some() {
        run.stage(host, "installing", "moving the previous identity aside");
        let out = privileged(session, elevate, &run.creds, plan.reset_script(), QUICK)
            .await
            .map_err(|e| Fail::from(("installing", e)))?;
        if !out.ok() {
            return Err(fail("installing", out.complaint()));
        }
    }

    run.check_cancel("installing")?;
    let staged = stage_binary(run, host, session, &probe, &plan, &bin).await?;
    let (service_written, install_notes) =
        install(run, host, session, &plan, elevate, staged.as_deref()).await?;
    let mut extra = install_notes;
    if service_written == "none" {
        extra.push(
            "no systemd there, so the daemon runs under nohup — it will not come back on its own \
             after a reboot"
                .to_string(),
        );
    }
    for n in extra {
        note = Some(match note {
            Some(m) => format!("{m}; {n}"),
            None => n,
        });
    }

    run.check_cancel("pairing")?;
    let (node_id, node_name) = pair(run, host, session, target, &plan, elevate).await?;
    let (service, verify_note, version) =
        start_and_verify(run, host, session, &plan, elevate, &node_id, &node_name).await?;
    let note = match (note, verify_note) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    Ok(Done {
        node_name,
        node_id,
        version: if version.is_empty() {
            bin.version.clone()
        } else {
            version
        },
        service,
        note,
    })
}

/// Run the pipeline over every host, `parallel` at a time, and account for
/// each one.
async fn drive<F, Fut>(
    run: Arc<Run>,
    hosts: Vec<String>,
    parallel: u32,
    what: &'static str,
    each: F,
) where
    F: Fn(Arc<Run>, String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Step<Done>> + Send + 'static,
{
    let gate = Arc::new(Semaphore::new(parallel.clamp(1, 32) as usize));
    let each = Arc::new(each);
    let mut tasks = Vec::new();
    for host in hosts {
        let run = run.clone();
        let gate = gate.clone();
        let each = each.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = gate.acquire_owned().await;
            if run.cancelled() {
                run.emit(DeployEvent::Failed {
                    host: host.clone(),
                    stage: "connecting".into(),
                    message: "stopped".into(),
                    hint: None,
                });
                return false;
            }
            match each(run.clone(), host.clone()).await {
                Ok(done) => {
                    run.emit(DeployEvent::Done {
                        host,
                        node_name: done.node_name,
                        node_id: done.node_id,
                        version: done.version,
                        service: done.service,
                        note: done.note,
                    });
                    true
                }
                Err(f) => {
                    run.emit(DeployEvent::Failed {
                        host,
                        stage: f.stage.into(),
                        message: f.message,
                        hint: f.hint,
                    });
                    false
                }
            }
        }));
    }
    let mut done = 0;
    let mut failed = 0;
    for t in tasks {
        match t.await {
            Ok(true) => done += 1,
            _ => failed += 1,
        }
    }
    // The one moment worth a notification: the person may have walked
    // away from a rack that takes ten minutes.
    crate::notify::attention(
        &run.app,
        &format!("{what} finished"),
        &match (done, failed, run.cancelled()) {
            (d, 0, false) => format!("{d} machine{} done", if d == 1 { "" } else { "s" }),
            (d, f, false) => format!("{d} done, {f} failed"),
            (d, f, true) => format!("stopped: {d} done, {f} failed"),
        },
    );
    run.emit(DeployEvent::Finished {
        done,
        failed,
        cancelled: run.cancelled(),
    });
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// What a deploy would do: the hosts it read, the ones it could not, and
/// where each architecture's binary would come from.
#[tauri::command]
pub async fn deploy_preview(
    app: AppHandle,
    state: State<'_, Director>,
    targets: Vec<String>,
    binary_path: Option<String>,
    port: u16,
) -> Result<DeployPreviewView, String> {
    let engine = state.engine().await?;
    let mut hosts = Vec::new();
    let mut problems = Vec::new();
    match deploy::expand_all(&targets) {
        Ok(list) => {
            for raw in list {
                // Normalised, exactly as the run will normalise it, so the
                // preview is a preview: the person sees `rack-1:2222` and
                // that string is what gets dialled and what keys the card.
                match Target::parse_with_default(&raw, port) {
                    Ok(t) => hosts.push(t.to_string()),
                    Err(e) => problems.push(e.to_string()),
                }
            }
        }
        Err(e) => problems.push(e.to_string()),
    }
    let binary = picked_path(&state.picked, binary_path.as_deref())?;
    let sources = describe_sources(&engine, &app, binary.as_deref()).await;
    Ok(DeployPreviewView {
        hosts,
        problems,
        sources,
        known_hosts: crate::ssh::known_hosts_store().display().to_string(),
    })
}

/// Start a deploy. Events arrive on `channel`; the returned id is what
/// `deploy_decide` and `deploy_cancel` address.
#[tauri::command]
pub async fn deploy_start(
    app: AppHandle,
    state: State<'_, Director>,
    draft: DeployDraft,
    channel: Channel<DeployEvent>,
) -> Result<u64, String> {
    let engine = state.engine().await?;
    let expanded = deploy::expand_all(&draft.targets).map_err(|e| e.to_string())?;
    // The run's port fills in every target that named none, and the
    // normalised form is what travels: the host string is the card key and
    // every later step re-parses it, so a port beside it rather than in it
    // would be a port dropped.
    let mut hosts = Vec::with_capacity(expanded.len());
    for raw in expanded {
        let t = Target::parse_with_default(&raw, draft.port).map_err(|e| e.to_string())?;
        hosts.push(t.to_string());
    }
    if hosts.is_empty() {
        return Err("no machines named".into());
    }
    for h in &hosts {
        Target::parse(h).map_err(|e| e.to_string())?;
    }
    // Tokens for what the person picked (SEC-14), resolved here.
    let binary = picked_path(&state.picked, draft.binary_path.as_deref())?;
    let creds = Creds::resolve(draft.auth, &hosts, &state.picked).await?;
    let cancel = CancellationToken::new();
    let decisions = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let run_id = {
        let mut inner = state.inner.lock().await;
        inner.next_deploy += 1;
        let id = inner.next_deploy;
        inner.deploys.insert(
            id,
            DeployHandle {
                decisions: decisions.clone(),
                cancel: cancel.clone(),
            },
        );
        id
    };
    let run = Arc::new(Run {
        engine: engine.clone(),
        channel,
        creds,
        tags: draft
            .tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        install_path: draft.install_path.filter(|p| !p.trim().is_empty()),
        fleet_upgrade: draft.fleet_upgrade,
        open_firewall: draft.open_firewall,
        replace_existing: draft.replace_existing,
        // Adding a machine installs whatever it does not have; there is no
        // version to skip past, so there is nothing to force.
        force: false,
        resolver: Resolver::new(engine, &app, binary),
        decisions,
        cancel,
        app: app.clone(),
    });
    run.emit(DeployEvent::Plan {
        run_id,
        hosts: hosts.clone(),
    });
    let parallel = draft.parallel;
    let handle = app.clone();
    tokio::spawn(async move {
        drive(run, hosts, parallel, "Add machines", deploy_host).await;
        let director = handle.state::<Director>();
        director.inner.lock().await.deploys.remove(&run_id);
    });
    Ok(run_id)
}

/// The person's verdict on one machine's code.
#[tauri::command]
pub async fn deploy_decide(
    state: State<'_, Director>,
    run_id: u64,
    host: String,
    accept: bool,
) -> Result<(), String> {
    let inner = state.inner.lock().await;
    let run = inner
        .deploys
        .get(&run_id)
        .ok_or_else(|| "that deploy is no longer running".to_string())?;
    let tx = run
        .decisions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&host)
        .ok_or_else(|| format!("{host} is not waiting for a decision"))?;
    let _ = tx.send(accept);
    Ok(())
}

/// Stop a deploy: hosts not started are skipped, hosts waiting on a code
/// are declined, hosts mid-install finish the step they are on.
#[tauri::command]
pub async fn deploy_cancel(state: State<'_, Director>, run_id: u64) -> Result<(), String> {
    let inner = state.inner.lock().await;
    if let Some(run) = inner.deploys.get(&run_id) {
        run.cancel.cancel();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Upgrade over SSH
// ---------------------------------------------------------------------------

/// Push a verified binary onto a node and restart its service — the path
/// for a node that cannot replace its own binary.
async fn upgrade_host(run: Arc<Run>, raw: String) -> Step<Done> {
    // `raw` is `node_id|target` — the node is what the mesh knows, the
    // target is how SSH reaches it.
    let (node_id, target_raw) = raw
        .split_once('|')
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .unwrap_or((String::new(), raw.clone()));
    let host = raw.as_str();
    let target = Target::parse(&target_raw).map_err(|e| fail("connecting", e))?;
    run.check_cancel("connecting")?;
    let session = open(&run, host, &target).await?;
    let result = upgrade_host_inner(&run, host, &session, &node_id).await;
    session.close().await;
    result
}

async fn upgrade_host_inner(run: &Run, host: &str, session: &Session, node_id: &str) -> Step<Done> {
    let probe = inspect(run, host, session, "/usr/local/bin/meshnode").await?;
    let exe = probe.exe.clone().ok_or_else(|| {
        fail(
            "inspecting",
            format!("no meshnode found on {}", probe.hostname),
        )
        .hint("Add it as a new machine instead.")
    })?;
    // A binary inside the account's home is a user install; anything else
    // is the machine's and needs root to replace.
    let user_scope = !probe.home.is_empty() && exe.starts_with(&probe.home);
    let priv_ = probe.privilege.unwrap_or(Privilege::None);
    let elevate = if user_scope {
        Privilege::None
    } else {
        match priv_ {
            Privilege::Root => Privilege::Root,
            Privilege::SudoNoPassword => Privilege::SudoNoPassword,
            Privilege::SudoPassword if run.creds.sudo_password.is_some() => Privilege::SudoPassword,
            _ => {
                return Err(fail(
                    "inspecting",
                    format!(
                        "{exe} is the machine's, and {} cannot reach root there",
                        probe.user
                    ),
                )
                .hint("Give a sudo password under Credentials."));
            }
        }
    };
    check_sudo(run, session, elevate, &probe.user).await?;
    let mut plan = if user_scope {
        InstallPlan::user(&probe.home, &probe.user)
    } else {
        InstallPlan::system(Some(&exe))
    };
    plan.install_path = exe.clone();
    plan.validate().map_err(|e| fail("preparing", e))?;
    run.emit(DeployEvent::Machine {
        host: host.to_string(),
        hostname: probe.hostname.clone(),
        distro: probe.distro.clone(),
        arch: probe.arch.clone(),
        scope: if user_scope { "user" } else { "system" }.into(),
        note: probe
            .version
            .clone()
            .map(|v| format!("running {v} at {exe}")),
    });

    run.check_cancel("preparing")?;
    run.stage(
        host,
        "preparing",
        format!("finding a meshnode for {}", probe.arch),
    );
    let bin = run
        .resolver
        .get(&probe.arch)
        .await
        .map_err(|e| fail("preparing", e))?;
    let same_version = probe.version_number().as_deref() == Some(bin.version.as_str());
    if same_version && !run.force {
        return Ok(Done {
            node_name: probe.hostname.clone(),
            node_id: node_id.to_string(),
            version: bin.version.clone(),
            service: if probe.service_active {
                "systemd".into()
            } else {
                "none".into()
            },
            note: Some(format!("already on {}", bin.version)),
        });
    }
    run.stage(
        host,
        "preparing",
        format!("meshnode {} · {}", bin.version, bin.detail),
    );

    run.check_cancel("installing")?;
    run.stage(
        host,
        "installing",
        format!(
            "sending meshnode {} ({:.1} MB)",
            bin.version,
            bin.bytes.len() as f64 / 1_048_576.0
        ),
    );
    let out = session
        .run(
            &sh_c(&InstallPlan::stage_script()),
            Some(bin.bytes.clone()),
            STEP,
        )
        .await
        .map_err(|e| Fail::from(("installing", e)))?;
    if !out.ok() {
        return Err(fail("installing", out.complaint()));
    }
    let staged = deploy::kv_value(&out.stdout, "staged")
        .ok_or_else(|| {
            fail(
                "installing",
                "the stage step did not say where it put the binary",
            )
        })?
        .to_string();

    run.stage(host, "starting", "swapping the binary and restarting");
    let out = privileged(
        session,
        elevate,
        &run.creds,
        plan.upgrade_script(&staged),
        STEP,
    )
    .await
    .map_err(|e| Fail::from(("starting", e)))?;
    if let Some(owner) = deploy::kv_value(&out.stdout, "owner") {
        return Err(fail(
            "installing",
            format!("{exe} is owned by {owner}, so the package manager upgrades it"),
        )
        .hint(if owner == "dpkg" {
            "On that machine: apt install meshnode, then systemctl restart meshnode."
        } else {
            "On that machine: dnf upgrade meshnode, then systemctl restart meshnode."
        }));
    }
    if !out.ok() {
        return Err(fail("starting", out.complaint()));
    }
    let service = match deploy::kv_value(&out.stdout, "restarted") {
        Some("systemd") => "systemd".to_string(),
        _ => "none".to_string(),
    };
    let mut note = (service == "none").then(|| {
        "the binary is in place but no service was found to restart — start it by hand there"
            .to_string()
    });

    // Verifying a *forced* push onto the same version is a question the
    // mesh cannot answer. The proof an ordinary upgrade waits for is the
    // node reporting the new version — and here the node reported that
    // version before the binary was replaced, so waiting for it would be
    // reading back the premise. What is left is what the machine itself
    // just said over SSH: the swap landed and the service restarted, which
    // is a first-hand report rather than an inference. Say exactly that,
    // and do not dress it up as a version check.
    if same_version {
        return Ok(Done {
            node_name: probe.hostname.clone(),
            node_id: node_id.to_string(),
            version: bin.version.clone(),
            service: service.clone(),
            note: Some(match service.as_str() {
                "systemd" => format!(
                    "reinstalled {} and restarted the service — the version is unchanged, so the \
                     mesh cannot confirm the swap for you",
                    bin.version
                ),
                _ => format!(
                    "reinstalled {} — no service was found to restart, so start it by hand there",
                    bin.version
                ),
            }),
        });
    }

    run.stage(
        host,
        "verifying",
        format!("waiting for the node to report {}", bin.version),
    );
    let deadline = tokio::time::Instant::now() + VERIFY_UPGRADE;
    let mut seen = false;
    let mut node_name = probe.hostname.clone();
    while tokio::time::Instant::now() < deadline && service != "none" {
        if let Ok(IpcResponse::Nodes(rows)) = run.engine.ipc(IpcRequest::Nodes).await
            && let Some(row) = rows
                .iter()
                .find(|r| r.node_id.to_display_string() == node_id)
        {
            node_name = row.name.clone();
            if matches!(row.liveness, Liveness::Alive)
                && row.version.as_deref() == Some(bin.version.as_str())
            {
                seen = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !seen && service != "none" {
        note = Some(format!(
            "restarted, but the mesh has not seen it on {} yet — it usually appears shortly",
            bin.version
        ));
    }
    Ok(Done {
        node_name,
        node_id: node_id.to_string(),
        version: bin.version.clone(),
        service,
        note,
    })
}

/// Upgrade nodes over SSH. Same events as a deploy; the card key is
/// `node_id|target`.
#[tauri::command]
pub async fn upgrade_ssh_start(
    app: AppHandle,
    state: State<'_, Director>,
    draft: SshUpgradeDraft,
    channel: Channel<DeployEvent>,
) -> Result<u64, String> {
    let engine = state.engine().await?;
    let IpcResponse::Nodes(rows) = engine.ipc(IpcRequest::Nodes).await? else {
        return Err("unexpected response".into());
    };
    let mut hosts = Vec::new();
    // Never this machine. Director keeps its node at the meshnode Director
    // carries (`localnode::keep_current`), and an SSH session into the
    // machine Director is running on — as root, with credentials nobody
    // saved for it — is never what "update the fleet" meant.
    let me = rows
        .iter()
        .find(|r| r.is_self)
        .map(|r| r.node_id.to_display_string());
    for n in draft
        .nodes
        .iter()
        .filter(|n| Some(&n.node_id) != me.as_ref())
    {
        let row = rows
            .iter()
            .find(|r| r.node_id.to_display_string() == n.node_id)
            .ok_or_else(|| format!("no such node: {}", n.node_id))?;
        let target = match n.host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) => h.to_string(),
            None => {
                // The node's first non-loopback address, minus the mesh
                // port: the machine SSH reaches is the machine the mesh
                // reaches, until somebody says otherwise.
                let addr = row
                    .addrs
                    .iter()
                    .find(|a| {
                        !a.starts_with("127.")
                            && !a.starts_with("[::1]")
                            && !a.starts_with("relay:")
                    })
                    .or(row.addrs.first())
                    .ok_or_else(|| format!("{} has no address to reach it at", row.name))?;
                addr.rsplit_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or(addr)
                    .trim_matches(['[', ']'])
                    .to_string()
            }
        };
        // Parsed with the run's port as the default, then carried
        // forward *normalised*: the card key is what every later step
        // re-parses, so the port has to survive in the string rather than
        // beside it. A row that named its own port keeps it.
        let target = Target::parse_with_default(&target, draft.port).map_err(|e| e.to_string())?;
        hosts.push(format!("{}|{target}", n.node_id));
    }
    if hosts.is_empty() {
        return Err(if draft.nodes.is_empty() {
            "no nodes named".into()
        } else {
            "this machine's node is not updated over SSH: Director updates it to the meshnode \
             it carries, each time Director starts"
                .into()
        });
    }
    // Tokens for what the person picked (SEC-14), resolved here.
    let binary = picked_path(&state.picked, draft.binary_path.as_deref())?;
    let creds = Creds::resolve(draft.auth, &hosts, &state.picked).await?;
    let cancel = CancellationToken::new();
    let decisions = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let run_id = {
        let mut inner = state.inner.lock().await;
        inner.next_deploy += 1;
        let id = inner.next_deploy;
        inner.deploys.insert(
            id,
            DeployHandle {
                decisions: decisions.clone(),
                cancel: cancel.clone(),
            },
        );
        id
    };
    let mut resolver = Resolver::new(engine.clone(), &app, binary);
    // A named version means that release from the mesh, exactly — unless a
    // file was picked, which outranks it. A picked file is the operator
    // pointing at the bytes they mean; resolving a version *around* it
    // would install something else and say it did what was asked.
    if let Some(v) = draft
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .filter(|_| resolver.explicit.is_none())
    {
        let want = v.trim_start_matches(['v', 'V']).to_string();
        if let IpcResponse::Releases(releases) = engine.ipc(IpcRequest::Releases).await? {
            let mut cache = HashMap::new();
            for r in releases.into_iter().filter(|r| r.version == want) {
                let data = bundle_file(&engine, r.bundle_id, &r.artifact).await?;
                let arch = if r.arch.is_empty() {
                    std::env::consts::ARCH.to_string()
                } else {
                    r.arch.clone()
                };
                cache.insert(
                    arch,
                    Arc::new(Binary {
                        bytes: data.into(),
                        version: r.version.clone(),
                        kind: "mesh",
                        detail: format!("release {} published in this mesh", r.version),
                    }),
                );
            }
            if cache.is_empty() {
                return Err(format!(
                    "{want} is not published in this mesh — publish it under Updates, or pick a \
                     binary file to push"
                ));
            }
            resolver.cache = Mutex::new(cache);
        }
    }
    let run = Arc::new(Run {
        engine,
        channel,
        creds,
        tags: Vec::new(),
        install_path: None,
        fleet_upgrade: true,
        open_firewall: false,
        replace_existing: false,
        force: draft.force,
        resolver,
        decisions,
        cancel,
        app: app.clone(),
    });
    run.emit(DeployEvent::Plan {
        run_id,
        hosts: hosts.clone(),
    });
    let parallel = draft.parallel;
    let handle = app.clone();
    tokio::spawn(async move {
        drive(run, hosts, parallel, "Update over SSH", upgrade_host).await;
        let director = handle.state::<Director>();
        director.inner.lock().await.deploys.remove(&run_id);
    });
    Ok(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_off_output_and_names() {
        assert_eq!(
            version_from_output(b"meshnode 0.6.0\n"),
            Some("0.6.0".into())
        );
        assert_eq!(
            version_from_output(b"meshnode v0.7.1"),
            Some("0.7.1".into())
        );
        assert_eq!(version_from_output(b"garbage"), None);
        assert_eq!(
            version_from_name(Path::new("/x/meshnode-v0.6.0-x86_64-unknown-linux-musl")),
            Some("0.6.0".into())
        );
        assert_eq!(version_from_name(Path::new("/x/meshnode")), None);
        assert!(version_cmp("0.10.0", "0.9.0").is_gt());
    }

    #[test]
    fn credentials_pick_a_sudo_password_sensibly() {
        let c = Creds::from_draft(DeployAuthDraft {
            user: "ops".into(),
            method: "password".into(),
            password: Some("pw".into()),
            key_path: None,
            passphrase: None,
            sudo_password: None,
            remember: false,
            from_keychain: false,
        })
        .expect("creds");
        assert_eq!(c.sudo_password.as_deref().map(String::as_str), Some("pw"));
        let c = Creds::from_draft(DeployAuthDraft {
            user: "".into(),
            method: "key".into(),
            password: None,
            key_path: Some("/k".into()),
            passphrase: None,
            sudo_password: Some("root-pw".into()),
            remember: false,
            from_keychain: false,
        })
        .expect("creds");
        assert_eq!(c.user, None);
        assert_eq!(
            c.sudo_password.as_deref().map(String::as_str),
            Some("root-pw")
        );
        assert!(
            Creds::from_draft(DeployAuthDraft {
                user: "ops".into(),
                method: "key".into(),
                password: None,
                key_path: None,
                passphrase: None,
                sudo_password: None,
                remember: false,
                from_keychain: false,
            })
            .is_err()
        );
    }

    #[test]
    fn joiner_frames_parse() {
        let f: JoinerFrame = serde_json::from_str(
            r#"{"event":"listening","port":7777,"addr":"0.0.0.0:7777","node_name":"rack-1"}"#,
        )
        .expect("listening");
        assert!(matches!(f, JoinerFrame::Listening { port: 7777, .. }));
    }
}
