//! `meshnode dns install` — make `<name>.mesh` resolve on this machine (DNS
//! hardening).
//!
//! **The machine's resolver has to send `.mesh` to the responder.** It
//! listens on 8600, not 53, so systemd-resolved is told to route `~mesh`
//! there — the drop-in Consul documents for `.consul`, for the same reason.
//! That needs root, and a node runs without it, so it is a step a person
//! takes once per machine.
//!
//! It also **takes back what the retired mesh proxy was given.** While the
//! proxy existed this install let it bind port 80: a `meshnode.service`
//! drop-in with `CAP_NET_BIND_SERVICE` for a systemd node, and — for
//! Director's node, which has no unit — the machine's unprivileged port floor
//! lowered to 0 for every process. Nothing binds a low port for a name any
//! more, so a machine that ran the old install has a privilege nothing uses,
//! and one of the two is machine-wide. Each is removed only where this
//! install's own file says it put it there.
//!
//! Everything here is rendered by pure functions, so a dry run prints exactly
//! what an install writes.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::error::{CoreError, Result};

pub const RESOLVED_DROPIN: &str = "/etc/systemd/resolved.conf.d/meshware.conf";
/// The retired mesh proxy's `CAP_NET_BIND_SERVICE` grant for a systemd node.
pub const LEGACY_SERVICE_DROPIN: &str = "/etc/systemd/system/meshnode.service.d/proxy.conf";
/// The retired mesh proxy's machine-wide port floor, for Director's node.
pub const LEGACY_SYSCTL_FILE: &str = "/etc/sysctl.d/60-meshware-proxy.conf";

/// The kernel's own unprivileged port floor, which the retired grant moved.
const DEFAULT_UNPRIVILEGED_PORT_START: u16 = 1024;

/// What an install writes, removes and runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<(PathBuf, String)>,
    pub removals: Vec<PathBuf>,
    pub commands: Vec<Vec<String>>,
}

/// Which of the retired mesh proxy's grants are on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Legacy {
    pub service_dropin: bool,
    pub sysctl: bool,
}

impl Legacy {
    pub fn detect() -> Legacy {
        Legacy {
            service_dropin: std::path::Path::new(LEGACY_SERVICE_DROPIN).exists(),
            sysctl: std::path::Path::new(LEGACY_SYSCTL_FILE).exists(),
        }
    }
}

/// The systemd-resolved drop-in that sends `.mesh` to the responder at `addr`.
///
/// `DNSSEC=false` only when the resolver validates: the root zone signs that
/// `.mesh` does not exist, so a validating resolver throws away every answer
/// the responder gives. A resolver already set to `no` is left as it is.
pub fn resolved_dropin(addr: SocketAddr, resolver_validates: bool) -> String {
    let mut out = String::from(
        "# Written by `meshnode dns install`: send .mesh names to this machine's\n\
         # meshware responder, and nothing else there.\n\
         [Resolve]\n",
    );
    out.push_str(&format!("DNS={addr}\n"));
    out.push_str(&format!("Domains=~{}\n", mesh_proto::DNS_SUFFIX));
    if resolver_validates {
        out.push_str(
            "# The root zone signs that .mesh does not exist, so a validating\n\
             # resolver would discard every .mesh answer.\n\
             DNSSEC=false\n",
        );
    }
    out
}

/// Does `resolvectl dnssec` say the resolver validates?
pub fn resolver_validates(dnssec_output: &str) -> bool {
    !dnssec_output
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("Global: no"))
}

pub fn plan(addr: SocketAddr, resolver_validates: bool, legacy: Legacy) -> Plan {
    let files = vec![(
        PathBuf::from(RESOLVED_DROPIN),
        resolved_dropin(addr, resolver_validates),
    )];
    let mut removals = Vec::new();
    let mut commands = vec![vec![
        "systemctl".to_string(),
        "restart".into(),
        "systemd-resolved".into(),
    ]];
    if legacy.service_dropin {
        removals.push(PathBuf::from(LEGACY_SERVICE_DROPIN));
        commands.push(vec!["systemctl".into(), "daemon-reload".into()]);
        commands.push(vec![
            "systemctl".into(),
            "try-restart".into(),
            "meshnode".into(),
        ]);
    }
    if legacy.sysctl {
        removals.push(PathBuf::from(LEGACY_SYSCTL_FILE));
        // Removing the file only stops it applying at the next boot; the
        // live value is put back too.
        commands.push(vec![
            "sysctl".into(),
            "-w".into(),
            format!("net.ipv4.ip_unprivileged_port_start={DEFAULT_UNPRIVILEGED_PORT_START}"),
        ]);
    }
    Plan {
        files,
        removals,
        commands,
    }
}

/// Run an install: needs root, and systemd-resolved.
pub fn install(addr: SocketAddr) -> Result<Vec<String>> {
    #[cfg(unix)]
    if !rustix::process::geteuid().is_root() {
        return Err(CoreError::State(format!(
            "this writes system resolver configuration and needs root: sudo meshnode dns install --address {addr}"
        )));
    }
    let resolved_active = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "systemd-resolved"])
        .status()
        .is_ok_and(|s| s.success());
    if !resolved_active {
        return Err(CoreError::State(format!(
            "systemd-resolved is not running here, so there is no drop-in to write. Point your \
             resolver's .{0} zone at {addr} — dnsmasq: `server=/{0}/{1}#{2}`; unbound: a \
             stub-zone for \"{0}.\" with stub-addr {1}@{2}",
            mesh_proto::DNS_SUFFIX,
            addr.ip(),
            addr.port()
        )));
    }
    let validates = std::process::Command::new("resolvectl")
        .arg("dnssec")
        .output()
        .map(|o| resolver_validates(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(true);
    let plan = plan(addr, validates, Legacy::detect());
    let mut done = Vec::new();
    for (path, body) in &plan.files {
        write_system_file(path, body)?;
        done.push(format!("wrote {}", path.display()));
    }
    for path in &plan.removals {
        std::fs::remove_file(path)?;
        done.push(format!(
            "removed {} (the retired mesh proxy's low-port grant)",
            path.display()
        ));
    }
    for command in &plan.commands {
        let ok = std::process::Command::new(&command[0])
            .args(&command[1..])
            .status()
            .is_ok_and(|s| s.success());
        done.push(format!(
            "{} {}",
            if ok { "ran" } else { "FAILED:" },
            command.join(" ")
        ));
    }
    // systemd-resolved has just been restarted; give it a moment to come back
    // before asking it anything, or a correct install reads as a failure.
    let mut routed = false;
    for _ in 0..20 {
        if crate::doctor::resolver_routes_mesh(addr) {
            routed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    done.push(if routed {
        format!(
            "this machine now sends .{} to {addr}",
            mesh_proto::DNS_SUFFIX
        )
    } else {
        format!(
            "the resolver still does not route .{} — `resolvectl domain` should list ~{} under \
             Global; `journalctl -u systemd-resolved` says why it did not load {}",
            mesh_proto::DNS_SUFFIX,
            mesh_proto::DNS_SUFFIX,
            RESOLVED_DROPIN
        )
    });
    Ok(done)
}

/// Write a system configuration file **readable by the services that read it**,
/// whatever this process's umask is.
///
/// The bug this exists for: Director makes everything it creates private
/// (umask 077) before anything else runs, including the `pkexec` re-execution
/// that installs this. The first install created `/etc/systemd/resolved.conf.d`
/// as 0700 and the drop-in as 0600; systemd-resolved runs as its own
/// unprivileged user, could not read either, and ignored the file without a
/// word — so `.mesh` never routed, and the install reported that it had written
/// everything. Directories are set to 0755 and files to 0644 explicitly, which
/// also repairs a directory a previous install left unreadable.
pub fn write_system_file(path: &std::path::Path, body: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resolver_is_told_to_send_mesh_here_and_nowhere_else() {
        let addr: SocketAddr = "127.0.0.1:8600".parse().unwrap();
        let quiet = resolved_dropin(addr, false);
        assert!(quiet.contains("DNS=127.0.0.1:8600\n"));
        assert!(
            quiet.contains("Domains=~mesh\n"),
            "a routing domain, not a search domain"
        );
        assert!(
            !quiet.contains("DNSSEC"),
            "a resolver already not validating is left alone"
        );
        assert!(resolved_dropin(addr, true).contains("DNSSEC=false"));
        assert!(!resolver_validates("Global: no\nLink 2 (eno1): no\n"));
        assert!(resolver_validates("Global: allow-downgrade\n"));
    }

    /// **The bug, as it happened.** A directory left 0700 by an install run
    /// under a private umask is made readable again, and the file with it.
    #[cfg(unix)]
    #[test]
    fn a_system_file_is_readable_by_the_service_that_reads_it() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("resolved.conf.d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = dir.join("meshware.conf");
        write_system_file(&file, "[Resolve]\n").unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&dir),
            0o755,
            "systemd-resolved runs unprivileged and must list the directory"
        );
        assert_eq!(mode(&file), 0o644);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[Resolve]\n");
    }

    /// An install grants nothing any more, and takes back each grant the
    /// mesh proxy's install left — only where it left one.
    #[test]
    fn the_retired_proxy_grants_are_taken_back_and_nothing_new_is_granted() {
        let addr: SocketAddr = "127.0.0.1:8600".parse().unwrap();
        let clean = plan(addr, false, Legacy::default());
        assert_eq!(clean.files.len(), 1, "only the resolver drop-in");
        assert!(clean.removals.is_empty());
        assert!(
            !clean
                .commands
                .iter()
                .flatten()
                .any(|w| w.contains("ip_unprivileged_port_start") || w == "meshnode"),
            "a machine that never had a grant is not touched: {:?}",
            clean.commands
        );

        let service = plan(
            addr,
            false,
            Legacy {
                service_dropin: true,
                sysctl: false,
            },
        );
        assert_eq!(service.removals, vec![PathBuf::from(LEGACY_SERVICE_DROPIN)]);
        assert!(
            service
                .commands
                .iter()
                .any(|c| c == &["systemctl", "daemon-reload"])
        );

        let desktop = plan(
            addr,
            false,
            Legacy {
                service_dropin: false,
                sysctl: true,
            },
        );
        assert_eq!(desktop.removals, vec![PathBuf::from(LEGACY_SYSCTL_FILE)]);
        assert!(
            desktop.commands.iter().any(|c| c.last().map(String::as_str)
                == Some("net.ipv4.ip_unprivileged_port_start=1024")),
            "the live value goes back too, not only the file: {:?}",
            desktop.commands
        );
    }
}
