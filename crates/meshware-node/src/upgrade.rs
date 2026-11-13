//! `meshnode upgrade --self` (MASTERPLAN M7, deferred from M6): fetch the
//! release binary for this architecture, verify its minisign signature
//! against the pinned release key, swap the binary atomically, then drain
//! the running daemon so systemd (`Restart=always`) brings the new one
//! up — all from the CLI process, which is the only one allowed to touch
//! `/usr/local/bin` (the daemon runs as `meshnode` under `ProtectSystem`).
//!
//! Refuses package-manager-owned binaries (`dpkg`/`rpm` own the file —
//! upgrade through them), refuses the placeholder key outside tests, and
//! never replaces anything it has not verified *and* executed (`--version`)
//! first.
//!
//! The binary it replaces is kept as `<exe>.prev`, and the new one is on
//! probation until the daemon has run it healthy for a minute: one that
//! does not get there in three starts is swapped back by itself, and
//! `meshnode upgrade --rollback` swaps back by hand (ENG-12,
//! [`mesh_core::upgrade_guard`]).
//!
//! Test seams (loud): `MESHWARE_RELEASE_BASE` (serve `<base>/latest` = tag
//! text and `<base>/<tag>/<artifact>[.minisig]`; plain `http://` allowed)
//! and `MESHWARE_MINISIGN_PUBKEY` (a base64 minisign public key).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use mesh_core::config::NodeConfig;
use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, read_frame};

use mesh_core::httpc as httpsc;

/// Release repository.
const REPO: &str = "neuralpunk/meshware";
/// Default release download base (GitHub redirects to object storage).
const DEFAULT_BASE: &str = "https://github.com/neuralpunk/meshware/releases/download";
/// Body cap for the binary download.
const MAX_BINARY: usize = 256 * 1024 * 1024;
/// How long the new binary may take to answer `--version`.
const SANITY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the daemon to come back with the new version.
const RESTART_WAIT: Duration = Duration::from_secs(90);
/// The pinned release signing key (single source of truth: CI asserts
/// `install.sh` carries the same line).
const EMBEDDED_PUBKEY_FILE: &str = include_str!("../../../packaging/minisign.pub");

#[derive(Debug, Clone, Default)]
pub struct UpgradeOptions {
    /// Report current vs latest and stop.
    pub check_only: bool,
    /// Explicit release tag (`v0.2.0` or `0.2.0`); default: latest.
    pub version: Option<String>,
    /// Pass `--move` to the drain.
    pub move_pending: bool,
    /// Install and stop — do not touch the running daemon (M24 §4).
    ///
    /// What a fleet upgrade uses: the daemon spawned this process and
    /// drains *itself* when it exits, because it is the only party that
    /// can authorize its own restart. This process signs IPC with whatever
    /// CLI identity the daemon's user happens to have, which on any node
    /// but the founder's is a key the mesh has never heard of.
    pub no_drain: bool,
    /// Skip the confirmation prompt.
    pub yes: bool,
    /// Binary to replace (default: the running executable).
    pub exe: Option<PathBuf>,
    /// Install **this already-fetched binary** instead of downloading one
    /// (M24 §9): the mesh delivered it peer-to-peer through the content
    /// store, and this is where it landed.
    ///
    /// The signature check is not skipped — it moves. `from_file` is
    /// verified against `minisig` before anything is staged, exactly as a
    /// download is, because the peer that served the bytes is a CDN and
    /// not an authority.
    pub from_file: Option<PathBuf>,
    /// The `.minisig` document for [`Self::from_file`].
    pub minisig: Option<String>,
    /// Release base URL (default: `MESHWARE_RELEASE_BASE`, else GitHub).
    pub release_base: Option<String>,
    /// Base64 minisign public keys to accept. Empty means the usual
    /// resolution: `MESHWARE_MINISIGN_PUBKEY`, else the embedded
    /// `packaging/minisign.pub`.
    ///
    /// A *list*, because trust is now mesh state (M24 §9): a fleet may be
    /// mid-rotation, with an old key still signing what is installed and a
    /// new key signing what is being published, and a node must accept
    /// either without anyone editing a file on it. The release is
    /// installed if **any** trusted key signed it — which is exactly what
    /// a set of trusted keys means, and no weaker than one key, since
    /// putting a key in that set is itself an admin-signed act.
    pub pubkeys: Vec<String>,
    /// The release's hardware co-signature, when the mesh delivered one
    /// (Y6). `None` on the release-channel path, which fetches
    /// `<artifact>.sshsig` beside the binary instead.
    ///
    /// Required only where the mesh trusts an SSH-form release key — a
    /// fleet that has never added one is unaffected, and one that has can
    /// no longer be upgraded by a key somebody copied out of CI.
    pub cosign: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonOutcome {
    /// No daemon answered on the IPC socket.
    NotRunning,
    /// The daemon already reported the target version.
    AlreadyCurrent,
    /// Drained; the daemon came back with the new version.
    Restarted,
    /// Drained and exited, but nothing restarted it within the wait.
    DrainedNotRestarted,
    /// The new version did not start, and the node put the previous one
    /// back (ENG-12). Carries what the node recorded.
    RolledBack(String),
    /// Nothing was installed, so the daemon was left alone.
    Untouched,
}

#[derive(Debug, Clone)]
pub struct UpgradeReport {
    pub current: String,
    pub target: String,
    pub installed: bool,
    pub exe: PathBuf,
    pub daemon: DaemonOutcome,
}

/// `meshnode-<tag>-<arch>-unknown-linux-musl` (the bare-binary release
/// artifact added in M7 next to the tarballs).
pub fn artifact_name(tag: &str) -> String {
    format!(
        "meshnode-{tag}-{}-unknown-linux-musl",
        std::env::consts::ARCH
    )
}

/// Normalize a user-supplied version to a release tag (`v1.2.3`).
pub fn normalize_tag(v: &str) -> String {
    let v = v.trim();
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// `(base, custom)`: an explicit/env base is a test seam — plain `http://`
/// is tolerated there, loudly.
fn release_base(explicit: Option<&str>) -> (String, bool) {
    let from_env = std::env::var("MESHWARE_RELEASE_BASE").ok();
    match explicit
        .map(str::to_string)
        .or(from_env)
        .filter(|b| !b.trim().is_empty())
    {
        Some(b) => {
            let b = b.trim().trim_end_matches('/').to_string();
            if b.starts_with("http://") {
                eprintln!(
                    "warning: release base {b} uses plain http — signatures are still verified, \
                     but this is a test seam, never use it for real upgrades"
                );
            }
            (b, true)
        }
        None => (DEFAULT_BASE.to_string(), false),
    }
}

/// The minisign public key: an explicit key, else the env seam, else the
/// embedded file. Refuses the placeholder key (a build that never had a
/// real key can only be upgraded by hand).
pub fn release_pubkey(explicit: Option<&str>) -> Result<String> {
    if let Some(k) = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("MESHWARE_MINISIGN_PUBKEY").ok())
        .filter(|k| !k.trim().is_empty())
    {
        return Ok(k.trim().to_string());
    }
    let key = EMBEDDED_PUBKEY_FILE
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment"))
        .ok_or_else(|| anyhow!("embedded packaging/minisign.pub has no key line"))?;
    if key.to_ascii_lowercase().contains("placeholder") {
        bail!(
            "this build carries the placeholder release key (packaging/minisign.pub), so it can \
             verify nothing on its own. Trust your signing key mesh-wide — Director → Nodes → \
             Updates → Release keys, or `meshnode upgrade --trust-key <file.pub>` — and every \
             node uses it, including this one. Or reinstall from \
             https://github.com/{REPO}/releases."
        );
    }
    Ok(key.to_string())
}

/// The keys to check a release against: those supplied (mesh-trusted keys
/// the daemon passed down, or an explicit `--release-key`), else the
/// single locally-resolved one.
///
/// Supplied keys are *added to*, not merged with, the local answer: a
/// machine whose owner wrote `[release] pubkey` gets exactly that, because
/// a written-down local answer outranks the mesh's.
pub fn release_pubkeys(explicit: &[String]) -> Result<Vec<String>> {
    let supplied: Vec<String> = explicit
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if !supplied.is_empty() {
        return Ok(supplied);
    }
    Ok(vec![release_pubkey(None)?])
}

/// Verifying a release is one question with one answer, and it lives in
/// `mesh_core::release` so the CLI, the daemon and Director cannot drift
/// apart on what counts as a valid one.
pub use mesh_core::release::{verify_minisign, verify_minisign_any};

/// Is `exe` owned by a package manager? Checks dpkg's file list for the
/// `meshnode` package and asks `rpm -qf`. Returns the manager's name.
pub fn package_owner(exe: &Path, dpkg_list: &Path) -> Option<&'static str> {
    if let Ok(list) = std::fs::read_to_string(dpkg_list) {
        let exe_s = exe.to_string_lossy();
        if list.lines().any(|l| l.trim() == exe_s) {
            return Some("dpkg");
        }
    }
    if let Ok(out) = std::process::Command::new("rpm")
        .arg("-qf")
        .arg(exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        && out.success()
    {
        return Some("rpm");
    }
    None
}

async fn resolve_latest(client: &httpsc::Client, base: &str, custom_base: bool) -> Result<String> {
    if custom_base {
        let resp = client.get(&format!("{base}/latest")).await?;
        let tag = String::from_utf8_lossy(&resp.body).trim().to_string();
        if tag.is_empty() {
            bail!("{base}/latest is empty");
        }
        return Ok(tag);
    }
    let resp = client
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .await
        .context("querying the latest release")?;
    let json: serde_json::Value =
        serde_json::from_slice(&resp.body).context("parsing the release listing")?;
    json.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("release listing has no tag_name"))
}

fn confirm(question: &str) -> bool {
    use std::io::Write as _;
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The whole flow. Prints progress to stderr; returns what happened.
pub async fn run(
    config: &NodeConfig,
    user: &Keypair,
    opts: UpgradeOptions,
) -> Result<UpgradeReport> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let (base, custom_base) = release_base(opts.release_base.as_deref());
    let client = httpsc::Client::new(custom_base && base.starts_with("http://"), MAX_BINARY)?;

    let tag = match &opts.version {
        Some(v) => normalize_tag(v),
        // A mesh-delivered upgrade always names its version; only the
        // release-channel path ever asks a server what "latest" is.
        None => resolve_latest(&client, &base, custom_base).await?,
    };
    let target = tag.trim_start_matches('v').to_string();
    let exe = match &opts.exe {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("locating the running binary")?,
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    eprintln!(
        "meshnode {current} installed at {}; latest release: {tag}",
        exe.display()
    );

    let mut report = UpgradeReport {
        current: current.clone(),
        target: target.clone(),
        installed: false,
        exe: exe.clone(),
        daemon: DaemonOutcome::Untouched,
    };
    if target == current && opts.version.is_none() {
        eprintln!("already up to date.");
        return Ok(report);
    }
    if opts.check_only {
        eprintln!(
            "upgrade available: {current} → {target}. Run `meshnode upgrade --self` to install."
        );
        return Ok(report);
    }

    // Package-manager-owned binaries are upgraded by the package manager.
    if let Some(owner) = package_owner(&exe, Path::new("/var/lib/dpkg/info/meshnode.list")) {
        bail!(
            "{} is owned by {owner} — upgrade with your package manager ({}), then \
             `meshnode drain` (systemd restarts it) or `systemctl restart meshnode`",
            exe.display(),
            if owner == "dpkg" {
                "apt install meshnode / dpkg -i meshnode_<ver>.deb"
            } else {
                "dnf upgrade meshnode / rpm -U meshnode-<ver>.rpm"
            }
        );
    }
    let pubkeys = release_pubkeys(&opts.pubkeys)?;

    // Obtain + verify. Two sources, one verification: whether the bytes
    // came off the mesh or off a release server, nothing is staged until
    // the release key has signed for them.
    let (bin, sig, cosign_doc) = match (&opts.from_file, &opts.minisig) {
        (Some(path), Some(sig)) => {
            eprintln!("using {} (delivered over the mesh)", path.display());
            let bin = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            (bin, sig.clone(), opts.cosign.clone())
        }
        _ => {
            let artifact = artifact_name(&tag);
            let url = format!("{base}/{tag}/{artifact}");
            // A spinner while the bytes come down, then the line that
            // stays. On a pipe the spinner draws nothing and the line is
            // all there is.
            let pb = spinner(format!("downloading {url}"));
            let bin = client
                .get(&url)
                .await
                .with_context(|| format!("downloading {artifact}"))?
                .body;
            let sig = client
                .get(&format!("{url}.minisig"))
                .await
                .with_context(|| format!("downloading {artifact}.minisig"))?
                .body;
            pb.finish_and_clear();
            eprintln!(
                "downloaded {artifact} ({:.1} MB)",
                bin.len() as f64 / 1_048_576.0
            );
            // Fetched best-effort: a release that has none is refused
            // below, by a message that says so, rather than here by a 404
            // from a CDN (Y6).
            let cosign = client
                .get(&format!("{url}.sshsig"))
                .await
                .ok()
                .and_then(|r| String::from_utf8(r.body).ok());
            (
                bin,
                String::from_utf8(sig).context("signature is not text")?,
                cosign,
            )
        }
    };
    // The two kinds of key live in one trusted list and are told apart by
    // shape — a minisign line starts `RW`, an SSH one names its algorithm.
    // One admin verb, two kinds of signature (Y6).
    let (ssh_keys, minisign_keys): (Vec<String>, Vec<String>) = pubkeys
        .iter()
        .cloned()
        .partition(|k| mesh_core::sshsig::is_ssh_key_line(k));
    let by = verify_minisign_any(&bin, &sig, &minisign_keys)?;
    eprintln!(
        "signature verified (minisign, {} bytes, key {}…)",
        bin.len(),
        by.chars().take(12).collect::<String>()
    );

    // Y6. **Opt-in, and fail-closed once opted into.** A fleet that trusts
    // no SSH-form release key behaves exactly as it did; one that trusts
    // any requires a second signature that a stolen CI secret cannot
    // produce, because it was made by a key on a device.
    if !ssh_keys.is_empty() {
        let cosign = match &cosign_doc {
            Some(doc) => doc.clone(),
            None => bail!(
                "this mesh requires a hardware co-signature on releases and this one carries \
                 none.\n  The minisign signature is valid; that is exactly the check a stolen \
                 signing key passes.\n  Publish one:  meshnode releases cosign {target} --sig \
                 <file>"
            ),
        };
        let line = mesh_core::sshsig::verify_release_cosignature(&cosign, &bin, &ssh_keys)?;
        let parsed = mesh_core::sshsig::parse(&cosign)?;
        eprintln!(
            "co-signature verified ({}, key {}…){}",
            if parsed.is_token_backed() {
                "hardware-backed"
            } else {
                "ssh"
            },
            line.split_whitespace()
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(12)
                .collect::<String>(),
            if parsed.user_present() {
                " — a human touched the key"
            } else {
                ""
            },
        );
    }

    if !opts.yes
        && !confirm(&format!(
            "upgrade meshnode {current} → {target} at {}?",
            exe.display()
        ))
    {
        bail!("aborted");
    }

    // Stage next to the target (same filesystem → atomic rename), make it
    // executable, and make sure it actually runs and is what it claims.
    let tmp = exe.with_file_name(format!(
        "{}.upgrade-{}",
        exe.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "meshnode".into()),
        std::process::id()
    ));
    let staged = stage_binary(&tmp, &bin, &target).await;
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Durable, and reversible: the binary being replaced is kept as
    // `<exe>.prev`, and both renames reach the disk before this goes on
    // (ENG-12).
    let prev = mesh_core::upgrade_guard::install(&tmp, &exe).map_err(|e| {
        // The refusal a fleet rollout hits on a hardened install, so it
        // names the fix rather than the errno. `ProtectSystem=full` makes
        // /usr read-only for the service on purpose; granting the one
        // directory back is an opt-in drop-in (M24 §4).
        let _ = std::fs::remove_file(&tmp);
        anyhow!(
            "cannot replace {}: {e}\n\
             The daemon runs unprivileged and the shipped systemd unit makes its install \
             directory read-only. To let this node upgrade itself, install the opt-in drop-in \
             (packaging/systemd/meshnode-fleet-upgrade.conf) with ReadWritePaths set to {}, \
             then reload systemd. Package installs upgrade through apt/dnf instead.",
            exe.display(),
            exe.parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the install directory".into()),
        )
    })?;
    report.installed = true;
    eprintln!(
        "installed meshnode {target} at {} (the previous binary is kept at {})",
        exe.display(),
        prev.display()
    );
    // On probation until the daemon has run it healthy. Without the
    // marker the upgrade still stands, but nothing would undo it.
    if let Err(e) = mesh_core::upgrade_guard::set_pending(
        &config.data_dir,
        &mesh_core::upgrade_guard::Pending {
            from: current.clone(),
            to: target.clone(),
            exe: exe.clone(),
            prev,
            starts: 0,
            installed_at_micros: mesh_proto::now_micros(),
        },
    ) {
        eprintln!(
            "warning: could not record the upgrade in {} ({e}) — if {target} does not start, \
             it will not be rolled back by itself; `meshnode upgrade --rollback` still works",
            config.data_dir.display()
        );
    }

    if opts.no_drain {
        eprintln!("installed; leaving the daemon to restart itself.");
        return Ok(report);
    }
    // Restart the daemon through a drain (systemd's Restart=always
    // brings the new binary up; otherwise the operator runs `meshnode up`).
    report.daemon = restart_daemon(config, user, &target, opts.move_pending).await;
    say_outcome(&report.daemon, &target);
    Ok(report)
}

fn say_outcome(outcome: &DaemonOutcome, target: &str) {
    match outcome {
        DaemonOutcome::NotRunning => {
            eprintln!(
                "no daemon on the local socket — run `meshnode up` (or start the service) to use {target}."
            );
        }
        DaemonOutcome::AlreadyCurrent => eprintln!("daemon already runs {target}."),
        DaemonOutcome::Restarted => eprintln!("daemon restarted and runs {target}."),
        DaemonOutcome::DrainedNotRestarted => {
            eprintln!(
                "daemon drained and exited but did not come back on its own — start it again \
                 (`systemctl start meshnode` or `meshnode up`)."
            );
        }
        DaemonOutcome::RolledBack(why) => eprintln!("error: {why}"),
        DaemonOutcome::Untouched => {}
    }
}

/// `meshnode upgrade --rollback`: put `<exe>.prev` back, and restart the
/// daemon onto it (ENG-12).
///
/// The binary it replaces is kept as `<exe>.failed`, and `.prev` is used
/// up: rolling back twice needs another upgrade in between.
pub async fn rollback(
    config: &NodeConfig,
    user: &Keypair,
    opts: UpgradeOptions,
) -> Result<UpgradeReport> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let exe = match &opts.exe {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("locating the running binary")?,
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    if let Some(owner) = package_owner(&exe, Path::new("/var/lib/dpkg/info/meshnode.list")) {
        bail!(
            "{} is owned by {owner} — roll back with your package manager (install the previous \
             package version)",
            exe.display()
        );
    }
    let prev = mesh_core::upgrade_guard::prev_path(&exe);
    if !prev.is_file() {
        bail!(
            "there is no previous binary at {} — only an upgrade made with `meshnode upgrade \
             --self` keeps one, and a rollback uses it up",
            prev.display()
        );
    }
    // Asked of the binary itself: the one being put back has to run.
    let out = tokio::time::timeout(
        SANITY_TIMEOUT,
        tokio::process::Command::new(&prev)
            .arg("--version")
            .output(),
    )
    .await
    .map_err(|_| anyhow!("{} did not answer `--version`", prev.display()))?
    .with_context(|| format!("running {}", prev.display()))?;
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        bail!(
            "{} does not run (`--version` exited {}) — not putting it back",
            prev.display(),
            out.status
        );
    }
    // The first line only: since DOC-14 `--version` goes on to print the
    // protocol, target and commit, and the version is the last word of the
    // first line, as it always was.
    let target = printed
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .trim_start_matches('v')
        .to_string();
    if !opts.yes
        && !confirm(&format!(
            "roll meshnode back {current} → {target} at {}?",
            exe.display()
        ))
    {
        bail!("aborted");
    }
    mesh_core::upgrade_guard::restore_prev(&exe).map_err(|e| {
        anyhow!(
            "cannot put {} back at {}: {e}",
            prev.display(),
            exe.display()
        )
    })?;
    // Whatever was on probation is not what runs now.
    let _ = mesh_core::upgrade_guard::clear_pending(&config.data_dir);
    eprintln!(
        "restored meshnode {target} at {} (the {current} binary is kept at {})",
        exe.display(),
        mesh_core::upgrade_guard::failed_path(&exe).display()
    );
    let mut report = UpgradeReport {
        current,
        target: target.clone(),
        installed: true,
        exe,
        daemon: DaemonOutcome::Untouched,
    };
    if opts.no_drain {
        return Ok(report);
    }
    report.daemon = restart_daemon(config, user, &target, opts.move_pending).await;
    say_outcome(&report.daemon, &target);
    Ok(report)
}

async fn stage_binary(tmp: &Path, bin: &[u8], target: &str) -> Result<()> {
    std::fs::write(tmp, bin).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", tmp.display()))?;
    }
    let out = tokio::time::timeout(
        SANITY_TIMEOUT,
        tokio::process::Command::new(tmp).arg("--version").output(),
    )
    .await
    .map_err(|_| anyhow!("new binary did not answer `--version` within {SANITY_TIMEOUT:?}"))?
    .context("running the new binary")?;
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !printed.contains(target) {
        bail!(
            "new binary sanity check failed: `--version` printed {printed:?} (expected {target}), \
             status {}",
            out.status
        );
    }
    Ok(())
}

async fn restart_daemon(
    config: &NodeConfig,
    user: &Keypair,
    target: &str,
    move_pending: bool,
) -> DaemonOutcome {
    let socket = config.socket_path();
    let status = mesh_core::ipc::request(&socket, user, IpcRequest::Status).await;
    let running_version = match status {
        Ok(IpcResponse::Status(s)) => s.version,
        // A decode error means an older daemon answered: still draining it
        // is right — it is exactly the thing we are replacing.
        Ok(_) | Err(mesh_core::CoreError::Proto(_)) => String::new(),
        Err(_) => return DaemonOutcome::NotRunning,
    };
    if running_version == target {
        return DaemonOutcome::AlreadyCurrent;
    }
    eprintln!(
        "draining the running daemon{} …",
        if move_pending {
            " (moving queued jobs)"
        } else {
            ""
        }
    );
    match mesh_core::ipc::request_stream(&socket, user, IpcRequest::Drain { move_pending }).await {
        Ok(mut stream) => loop {
            match read_frame::<_, IpcResponse>(&mut stream).await {
                Ok(IpcResponse::DrainProgress {
                    running,
                    queued,
                    moved,
                    devices_held,
                }) => {
                    eprintln!(
                        "  {running} running, {queued} queued, {moved} moved{}",
                        if devices_held > 0 {
                            format!(", {devices_held} device(s) still held")
                        } else {
                            String::new()
                        }
                    );
                }
                Ok(IpcResponse::Ok) | Err(_) => break,
                Ok(IpcResponse::Denied { message }) | Ok(IpcResponse::Error { message }) => {
                    eprintln!("drain refused: {message} — restart the daemon by hand");
                    return DaemonOutcome::DrainedNotRestarted;
                }
                Ok(_) => {}
            }
        },
        Err(e) => {
            eprintln!("cannot drain the daemon ({e}) — restart it by hand");
            return DaemonOutcome::DrainedNotRestarted;
        }
    }
    // Wait for something (systemd) to bring the new version up — or, if
    // it cannot start, for the node to put the old one back (ENG-12).
    let pb = spinner(format!("waiting for the daemon to come back on {target}"));
    let asked_at = mesh_proto::now_micros();
    let deadline = std::time::Instant::now() + RESTART_WAIT;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(r) = mesh_core::upgrade_guard::rolled_back(&config.data_dir)
            && r.failed == target
            && r.at_micros >= asked_at
        {
            pb.finish_and_clear();
            return DaemonOutcome::RolledBack(format!(
                "meshnode {target} started {} time(s) without becoming healthy, and the node \
                 put {} back. The binary that failed is kept at {}; `journalctl -u meshnode` \
                 says why it did not start.",
                r.starts,
                r.restored,
                r.failed_exe.display()
            ));
        }
        if let Ok(IpcResponse::Status(s)) =
            mesh_core::ipc::request(&socket, user, IpcRequest::Status).await
            && s.version == target
        {
            pb.finish_and_clear();
            return DaemonOutcome::Restarted;
        }
    }
    pb.finish_and_clear();
    DaemonOutcome::DrainedNotRestarted
}

/// A spinner on stderr with a message; invisible off a terminal.
fn spinner(message: String) -> indicatif::ProgressBar {
    let pb =
        indicatif::ProgressBar::with_draw_target(None, indicatif::ProgressDrawTarget::stderr());
    pb.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    pb.set_message(message);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_names() {
        assert_eq!(normalize_tag("0.2.0"), "v0.2.0");
        assert_eq!(normalize_tag(" v0.2.0 "), "v0.2.0");
        assert!(artifact_name("v0.2.0").starts_with("meshnode-v0.2.0-"));
        assert!(artifact_name("v0.2.0").ends_with("-unknown-linux-musl"));
    }

    #[test]
    fn package_owner_detection() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("meshnode");
        std::fs::write(&exe, b"x").unwrap();
        let list = dir.path().join("meshnode.list");
        std::fs::write(
            &list,
            format!("/usr/share/doc/meshnode\n{}\n", exe.display()),
        )
        .unwrap();
        assert_eq!(package_owner(&exe, &list), Some("dpkg"));
        std::fs::write(&list, "/usr/bin/other\n").unwrap();
        // rpm may or may not exist here; a non-owned file must not report dpkg.
        assert_ne!(package_owner(&exe, &list), Some("dpkg"));
        assert_ne!(
            package_owner(&exe, dir.path().join("missing").as_path()),
            Some("dpkg")
        );
    }

    #[test]
    fn embedded_key_is_parsed_or_refused() {
        // An explicit key always wins.
        assert_eq!(release_pubkey(Some(" RWTabc ")).unwrap(), "RWTabc");
        // With the placeholder key shipped in-tree, the embedded path must
        // refuse (unless the environment injects a key).
        if std::env::var("MESHWARE_MINISIGN_PUBKEY").is_err() {
            let embedded = release_pubkey(None);
            if EMBEDDED_PUBKEY_FILE
                .to_ascii_lowercase()
                .contains("placeholder")
            {
                assert!(embedded.is_err());
            } else {
                assert!(embedded.is_ok());
            }
        }
    }
}
