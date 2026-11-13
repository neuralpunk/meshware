//! This account's own node, run as a `systemctl --user` service.
//!
//! **Why a service.** Director used to run this machine's node inside the
//! app. Quitting the window took the machine out of the mesh, and a
//! `meshnode` typed afterwards found no node to talk to — and, told to start
//! one, founded a second mesh beside the real one. A node is now a process
//! of its own on every machine, desktops included, and Director and the CLI
//! are both views onto it over its socket. Either can be closed without the
//! other noticing.
//!
//! **The same unit everywhere.** What is written here is
//! [`InstallPlan::user`]'s: the unit Director's *Add machines* and `meshnode
//! adopt` put on a machine reached over SSH. A desktop's node and a rack's
//! differ in which account they run as and nothing else, so there is one
//! set of instructions for stopping, restarting and reading the log of any
//! of them.
//!
//! **Where there is no systemd user manager** — a container, a session
//! without one — the node is started as an ordinary detached process
//! instead. It then lasts until logout or a reboot rather than coming back
//! by itself, and [`Started`] says which of the two happened so the caller
//! can say so too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::deploy::{self, InstallPlan};
use crate::error::{CoreError, Result};

/// The unit's name, as `systemctl --user` spells it.
pub const UNIT: &str = "meshnode";

/// How this account's node was started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// By the `systemctl --user` unit: restarted if it dies, started again
    /// at the next login.
    Service,
    /// As a detached process, because there is no user manager to run a
    /// unit. Lasts until it is stopped, the session ends or the machine
    /// restarts.
    Process {
        pid: u32,
        /// Where its output goes.
        log: PathBuf,
    },
}

/// `$XDG_CONFIG_HOME/systemd/user/meshnode.service` — where the unit lives
/// once installed.
pub fn unit_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("systemd").join("user").join("meshnode.service"))
}

/// Has this account's node been installed as a service?
pub fn installed() -> bool {
    unit_path().is_some_and(|p| p.exists())
}

/// Is there a systemd user manager to hand a unit to?
pub fn systemd_available() -> bool {
    Path::new("/run/systemd/system").is_dir() && systemctl(&["show-environment"]).is_ok()
}

/// Is the unit running right now?
pub fn active() -> bool {
    systemctl(&["is-active", "--quiet", UNIT]).is_ok()
}

/// Where the installed binary goes: `~/.local/bin/meshnode`.
pub fn install_path() -> Option<PathBuf> {
    Some(PathBuf::from(plan()?.install_path))
}

/// The home the unit runs the node in: `~/.meshware`.
pub fn home() -> Option<PathBuf> {
    Some(PathBuf::from(plan()?.data_dir))
}

/// Put `binary` at [`install_path`] and write, enable and start the unit.
///
/// `binary` may already *be* the install path — a `meshnode` someone copied
/// there by hand, installing itself — in which case nothing is copied and
/// everything else still happens, exactly as `install_script(None)` does on
/// a remote machine.
///
/// Where there is no user manager, the binary is still installed and the
/// node is started as a process; see the module docs.
pub fn install(binary: &Path) -> Result<Started> {
    let plan = plan().ok_or_else(|| CoreError::Config("no home directory".into()))?;
    let target = PathBuf::from(&plan.install_path);
    let same = match (
        std::fs::canonicalize(binary),
        std::fs::canonicalize(&target),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    // Staged *beside* the target, so the script's `mv -f` is a rename on one
    // filesystem and a half-copied binary is never at the path the unit
    // runs.
    let staged =
        (!same).then(|| target.with_file_name(format!(".meshnode.staged-{}", std::process::id())));
    if let Some(staged) = &staged {
        if let Some(dir) = staged.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(binary, staged).map_err(|e| {
            CoreError::Config(format!(
                "copying {} to {}: {e}",
                binary.display(),
                staged.display()
            ))
        })?;
    }
    let script = plan.install_script(staged.as_deref().and_then(Path::to_str));
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .output()?;
    if let Some(staged) = &staged {
        // Moved by the script when it succeeded; gone either way.
        let _ = std::fs::remove_file(staged);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match installed_as(out.status.success(), &stdout) {
        Some(true) => {
            systemctl(&["start", UNIT])?;
            Ok(Started::Service)
        }
        Some(false) => spawn(&target, Path::new(&plan.data_dir)),
        None => Err(CoreError::Config(format!(
            "installing this machine's node failed: {} {}",
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// What the install script said it did: `Some(true)` for a unit the user
/// manager now has, `Some(false)` for a binary installed with no manager to
/// run it, `None` for a script that did not finish.
///
/// The script speaks `mw.key=value` lines, and [`deploy::kv_value`] takes
/// the key *without* its `mw.` — asking for `mw.installed` finds nothing,
/// which read a successful install as a failed one.
fn installed_as(exited_ok: bool, stdout: &str) -> Option<bool> {
    if !exited_ok || deploy::kv_value(stdout, "installed") != Some("ok") {
        return None;
    }
    Some(deploy::kv_value(stdout, "service") == Some("systemd-user"))
}

/// Start this account's node: the unit if there is one, otherwise
/// `binary` as a detached process.
///
/// Installing is [`install`]'s job and deliberately not a side effect of
/// this: writing a unit and a binary into somebody's home is a decision,
/// and "start" is not the word for it.
pub fn start(binary: &Path, home: &Path) -> Result<Started> {
    if installed() && systemd_available() {
        systemctl(&["start", UNIT])?;
        return Ok(Started::Service);
    }
    spawn(binary, home)
}

/// Stop the unit. `Ok` when there is no unit: nothing was running under it.
pub fn stop() -> Result<()> {
    if !installed() {
        return Ok(());
    }
    systemctl(&["stop", UNIT]).map(|_| ())
}

/// Restart the unit — what makes a newly installed binary the one running,
/// since `systemctl start` does nothing to a unit that already is.
pub fn restart() -> Result<()> {
    systemctl(&["restart", UNIT]).map(|_| ())
}

/// Have the running unit re-read `meshnode.toml`, applying what can change
/// live (`systemctl --user reload`, which is a SIGHUP). `Ok(false)` when
/// there is no running unit to ask: the change then applies at the node's
/// next start, and the caller should say so.
pub fn reload() -> Result<bool> {
    if !installed() || !active() {
        return Ok(false);
    }
    systemctl(&["reload", UNIT]).map(|_| true)
}

/// `binary up` for `home`, as a detached process, for a home the unit does
/// not run — a demo's, or one named by `$MESHWARE_HOME`.
pub fn run_detached(binary: &Path, home: &Path) -> Result<Started> {
    spawn(binary, home)
}

/// The plan for this account: `~/.local/bin`, `~/.meshware`, a user unit.
fn plan() -> Option<InstallPlan> {
    let home = dirs::home_dir()?;
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    Some(InstallPlan::user(home.to_str()?, &user))
}

/// `binary up` as a process of its own, outliving whoever started it.
///
/// Its own process group, so a terminal's Ctrl-C and Director's exit reach
/// the parent and not the node; output to `<home>/logs/meshnode.out`, since
/// there is no journal to catch it.
fn spawn(binary: &Path, home: &Path) -> Result<Started> {
    use std::os::unix::process::CommandExt as _;
    let logs = crate::layout::Layout::new(home).logs_dir();
    std::fs::create_dir_all(&logs)?;
    let log = logs.join("meshnode.out");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let child = Command::new(binary)
        .arg("up")
        .env(crate::layout::HOME_ENV, home)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0)
        .spawn()
        .map_err(|e| CoreError::Config(format!("starting {}: {e}", binary.display())))?;
    Ok(Started::Process {
        pid: child.id(),
        log,
    })
}

fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(CoreError::Config(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read the way the script writes it — `InstallPlan::install_script`'s
    /// user branch, whose last lines these are.
    #[test]
    fn the_install_script_is_read_the_way_it_writes() {
        let service = "mw.enabled=yes\nmw.service=systemd-user\nmw.installed=ok\n";
        assert_eq!(installed_as(true, service), Some(true));
        let bare = "mw.service=none\nmw.installed=ok\n";
        assert_eq!(installed_as(true, bare), Some(false));
        // Cut short, or exited non-zero: not installed, whatever it printed.
        assert_eq!(installed_as(true, "mw.service=systemd-user\n"), None);
        assert_eq!(installed_as(false, service), None);
        // …and the script really does end that way.
        let script = InstallPlan::user("/home/pi", "pi").install_script(None);
        assert!(script.contains("echo mw.service=systemd-user"), "{script}");
        assert!(script.contains("echo mw.installed=ok"), "{script}");
    }
}
