//! `meshnode demo` — a real mesh on one machine, gone when you stop it.
//!
//! **L1 P2.** The floor to seeing meshware work was *owning a second Linux
//! machine*, which is a wall no amount of documentation gets through. This
//! command removes it: `meshnode demo` founds an ephemeral mesh of real
//! `meshnode` daemons on loopback, joins them to each other, fans a command
//! out across all of them, and removes every trace when it stops.
//!
//! Three decisions from `docs/L1-L5-LEGIBILITY.md` shape the whole file:
//!
//! - **D-L1.1 — real nodes, never the mock.** Director's `mock.ts` makes
//!   beautiful deterministic screenshots and is the wrong substrate here: a
//!   person evaluating the product would be evaluating a fiction. These are
//!   the shipped daemon, the shipped scheduler and the shipped transport.
//!   If that is slower or uglier than a mock, the mock was lying.
//! - **D-L1.2 — loopback-only and ephemeral.** `127.0.0.1`, a scratch root
//!   that is never `~/.meshware`, no mDNS announce, no advertised WAN
//!   candidates. A demo node must not be able to join a real mesh, or be
//!   joined by one.
//! - **Supervision is the hard part.** `bench/harness/mesh.sh` uses `nohup`
//!   and accepts that a hard kill leaves orphans. A shipped command cannot,
//!   so this one owns its children in-process, arms `PR_SET_PDEATHSIG` on
//!   each of them, and reaps whatever a previous invocation left behind
//!   before it starts a new one.
//!
//! The genesis race is the trap this file exists to not re-discover. The
//! daemon bootstraps identity and genesis on first run, and *any* CLI call
//! issued before that lands races it into minting a different user key —
//! after which every later call answers "unknown or revoked user". Every
//! wait here polls for the files first and the socket second, which is what
//! `mesh.sh`'s `wait_ready` learned to do.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mesh_core::identity::Keypair;
use mesh_core::{ipc, layout::Layout, membership};
use mesh_proto::{IpcRequest, IpcResponse};
use serde::{Deserialize, Serialize};

/// What a demo mesh calls itself. Short on purpose: it is a label on every
/// screen the demo produces, and "demo" is the honest one.
const MESH_NAME: &str = "demo";

/// The command the demo fans out to prove itself. Prints one line per
/// machine, needs nothing installed, and changes nothing.
const PROOF: &str = "uname -a";

/// Ports are probed, never assumed, but the search has to start somewhere.
/// Deliberately clear of the default 7777 (mesh) and 7776 (observability)
/// so a demo never contends with a real node on the same machine.
const MESH_PORT_BASE: u16 = 7901;
const HTTP_PORT_BASE: u16 = 7951;

/// Configuration that would otherwise leak from the operator's shell into
/// every demo node and quietly break it — a `MESHWARE_LISTEN` pointing at
/// the real node's port, a `MESHWARE_HOME` pointing at `~/.meshware`.
const SCRUBBED_ENV: [&str; 11] = [
    "MESHWARE_HOME",
    "MESHWARE_DATA_DIR",
    "MESHWARE_LISTEN",
    "MESHWARE_NODE_NAME",
    "MESHWARE_MESH_NAME",
    "MESHWARE_HTTP",
    // Every demo node would otherwise try to bind the one API address.
    "MESHWARE_API",
    "MESHWARE_TAGS",
    "MESHWARE_ADVERTISE",
    "MESHWARE_RELAY",
    "MESHWARE_USER_IDENTITY",
];

pub struct Options {
    pub nodes: usize,
}

/// What `--stop` and the next invocation's reaper read. Written before the
/// first child is spawned and rewritten as each one starts, so a demo that
/// is killed halfway still leaves a complete record of what to clean up.
#[derive(Serialize, Deserialize, Default)]
struct State {
    mesh_name: String,
    started_at_micros: u64,
    nodes: Vec<NodeState>,
}

#[derive(Serialize, Deserialize, Clone)]
struct NodeState {
    name: String,
    dir: PathBuf,
    port: u16,
    http_port: u16,
    pid: u32,
}

/// One demo node: its identity on disk and the child process running it.
struct Node {
    state: NodeState,
    child: tokio::process::Child,
}

impl Node {
    fn socket(&self) -> PathBuf {
        mesh_core::config::socket_path_for(&self.state.dir)
    }
}

/// Where a demo lives. **Never** `~/.meshware` (D-L1.2).
///
/// `$XDG_RUNTIME_DIR` first because it is per-user, tmpfs-backed and wiped
/// at logout — which is the lifetime this directory wants. The temp-dir
/// fallback is namespaced by uid so two people demoing on one box do not
/// collide, and so neither can reap the other's nodes.
fn scratch_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir.join("meshware-demo");
        }
    }
    #[cfg(unix)]
    let who = rustix::process::getuid().as_raw().to_string();
    #[cfg(not(unix))]
    let who = "user".to_string();
    std::env::temp_dir().join(format!("meshware-demo-{who}"))
}

fn state_file(root: &Path) -> PathBuf {
    root.join("demo.json")
}

fn read_state(root: &Path) -> Option<State> {
    let text = std::fs::read_to_string(state_file(root)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(root: &Path, state: &State) -> Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(state_file(root), text)
        .with_context(|| format!("writing {}", state_file(root).display()))
}

/// Is this pid still a meshnode daemon of *this* demo?
///
/// A recorded pid is never trusted on its own. Pids are recycled, and a
/// demo that was `kill -9`'d last week must not take somebody's editor with
/// it today, so the process is confirmed to be running out of this scratch
/// root before it is signalled. Where `/proc` is unavailable the answer is
/// "no" and the pid is dropped rather than guessed at — leaking a process
/// is bad; killing an innocent one is worse.
fn is_ours(pid: u32, root: &Path) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    argv.contains("meshnode") && argv.contains(&*root.to_string_lossy())
}

#[cfg(unix)]
fn signal(pid: u32, sig: rustix::process::Signal) {
    if let Ok(pid) = i32::try_from(pid)
        && let Some(pid) = rustix::process::Pid::from_raw(pid)
    {
        let _ = rustix::process::kill_process(pid, sig);
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _sig: ()) {}

/// Kill whatever a previous demo left behind, and say how many.
///
/// This runs at the *start* of every demo rather than only at the end of
/// one, because the end of one is exactly the code path a `kill -9` skips.
/// Together with `PR_SET_PDEATHSIG` on each child and `kill_on_drop`, it
/// closes every way a demo can outlive the command that started it.
async fn reap_previous(root: &Path) -> usize {
    let Some(state) = read_state(root) else {
        return 0;
    };
    let mine: Vec<u32> = state
        .nodes
        .iter()
        .map(|n| n.pid)
        .filter(|pid| is_ours(*pid, root))
        .collect();
    if mine.is_empty() {
        return 0;
    }
    #[cfg(unix)]
    {
        for pid in &mine {
            signal(*pid, rustix::process::Signal::TERM);
        }
        // Give them a moment to go politely, then insist.
        for _ in 0..30 {
            if !mine.iter().any(|pid| is_ours(*pid, root)) {
                return mine.len();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for pid in &mine {
            signal(*pid, rustix::process::Signal::KILL);
        }
    }
    mine.len()
}

/// `meshnode demo --stop`, from any shell.
pub async fn stop() -> Result<i32> {
    let root = scratch_root();
    let killed = reap_previous(&root).await;
    let existed = root.exists();
    if existed {
        std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    match (killed, existed) {
        (0, false) => println!("no demo is running."),
        (0, true) => println!("no demo was running — removed {}", root.display()),
        (n, _) => println!(
            "demo stopped — {n} node(s) shut down and {} removed",
            root.display()
        ),
    }
    Ok(0)
}

/// A free port at or above `from`, skipping any already claimed here.
///
/// Probed rather than assumed. The mesh listener is QUIC, so its port is a
/// UDP one; the observability listener is HTTP, so its port is a TCP one,
/// and they are searched separately because a machine can easily have one
/// free and not the other.
fn free_port(from: u16, udp: bool, taken: &mut Vec<u16>) -> Result<u16> {
    for port in from..from.saturating_add(200) {
        if taken.contains(&port) {
            continue;
        }
        let addr = format!("127.0.0.1:{port}");
        let free = if udp {
            std::net::UdpSocket::bind(&addr).is_ok()
        } else {
            std::net::TcpListener::bind(&addr).is_ok()
        };
        if free {
            taken.push(port);
            return Ok(port);
        }
    }
    bail!(
        "no free {} port near {from} on 127.0.0.1 — something is using the whole range",
        if udp { "UDP" } else { "TCP" }
    )
}

/// One node's `meshnode.toml`.
///
/// The `[http]` stanza is the one line this file cannot omit. The default
/// observability address is a fixed `127.0.0.1:7776`, so without a distinct
/// one per node only the first node binds it and every other logs a warning
/// and runs without `/healthz` or `/metrics` — the failure `mesh.sh`
/// documents and the reason it writes the same stanza.
fn config_toml(name: &str, port: u16, http_port: u16, slots: usize) -> String {
    format!(
        "# Written by `meshnode demo`. Everything under this directory is\n\
         # ephemeral: it is removed when the demo stops.\n\
         node_name = \"{name}\"\n\
         \n\
         [mesh]\n\
         name = \"{MESH_NAME}\"\n\
         \n\
         # Loopback only, and nothing advertised: a demo node cannot join a\n\
         # real mesh and cannot be joined by one (D-L1.2).\n\
         [listen]\n\
         addr = \"127.0.0.1\"\n\
         port = {port}\n\
         \n\
         [log]\n\
         level = \"warn\"\n\
         \n\
         # One observability port per node — see the module comment.\n\
         [http]\n\
         addr = \"127.0.0.1:{http_port}\"\n\
         \n\
         [limits]\n\
         slots = {slots}\n\
         queue = 256\n\
         min_free_mem_mb = 0\n"
    )
}

/// Strip the operator's meshware environment out of a child's.
fn scrub_env(cmd: &mut tokio::process::Command) {
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
}

fn spawn_node(exe: &Path, node: &NodeState) -> Result<tokio::process::Child> {
    let log_path = node.dir.join("daemon.log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--data-dir")
        .arg(&node.dir)
        .arg("up")
        .arg("--supervised")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        // The last line of defence: a panic here still takes the children.
        .kill_on_drop(true);
    scrub_env(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("starting {}", node.name))
}

/// Ask the kernel to `SIGTERM` this process when the one that started it
/// dies. `meshnode up --supervised` calls this and nothing else does.
///
/// This is the last hole in the supervision story: `kill_on_drop` cannot
/// run if the demo is itself `SIGKILL`ed, and neither can a Ctrl-C handler.
/// Only the kernel can cover that, and `PR_SET_PDEATHSIG` is how it does.
///
/// It is a **flag on the child** rather than an `unsafe` pre-exec closure
/// in the parent, for the same reason `sessions::pty_shim` is a re-exec:
/// this codebase denies `unsafe_code` workspace-wide, and a setting the
/// child applies to itself in an ordinary `main` needs none. A flag also
/// does not leak the way an environment variable would — a job this daemon
/// later spawns must not inherit a death signal aimed at the demo.
pub fn arm_parent_death_signal() {
    #[cfg(target_os = "linux")]
    if let Err(e) =
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
    {
        // Not fatal: the reaper in the *next* `meshnode demo` and
        // `demo --stop` both still work. Say so rather than pretend.
        tracing::warn!("could not arm the parent-death signal: {e}");
    }
}

/// A step in progress. Redraws in place on a terminal; says nothing into a
/// pipe, where [`step_done`] alone is the record.
fn step(line: &str) {
    use std::io::{IsTerminal, Write};
    let mut err = std::io::stderr();
    if err.is_terminal() {
        let _ = write!(err, "\r  {line:<56}");
        let _ = err.flush();
    }
}

/// A step that finished. One clean line either way — a log full of
/// carriage returns is not a log.
fn step_done(line: &str) {
    use std::io::{IsTerminal, Write};
    let mut err = std::io::stderr();
    if err.is_terminal() {
        let _ = writeln!(err, "\r  {line:<56}");
    } else {
        let _ = writeln!(err, "  {line}");
    }
}

/// The last 20 lines of a node's log, for a failure message that names a
/// cause instead of a timeout.
fn log_tail(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(20).collect();
    lines
        .into_iter()
        .rev()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait for a node to have bootstrapped, then for it to answer.
///
/// Files first, socket second — the order is the whole point. See the
/// module comment: asking a half-bootstrapped daemon anything mints a
/// second user key and poisons the rest of the run.
async fn wait_ready(node: &mut Node, deadline: Duration) -> Result<Keypair> {
    let layout = Layout::new(&node.state.dir);
    let genesis = layout.mesh_dir().join("genesis.json");
    let user_key = mesh_core::identity::cli_user_identity_path(&node.state.dir, None);
    let started = Instant::now();
    let bootstrapped = |p: &Path| p.metadata().map(|m| m.len() > 0).unwrap_or(false);

    while started.elapsed() < deadline {
        if let Some(status) = node.child.try_wait()? {
            bail!(
                "{} exited before it was ready ({status}). Its log said:\n{}",
                node.state.name,
                log_tail(&node.state.dir)
            );
        }
        if bootstrapped(&genesis) && bootstrapped(&user_key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !(bootstrapped(&genesis) && bootstrapped(&user_key)) {
        bail!(
            "{} never wrote its identity and genesis. Its log said:\n{}",
            node.state.name,
            log_tail(&node.state.dir)
        );
    }

    let user = mesh_core::identity::load_cli_user(&node.state.dir, None)
        .with_context(|| format!("loading {}'s user key", node.state.name))?;
    let socket = node.socket();
    while started.elapsed() < deadline {
        if ipc::request(&socket, &user, IpcRequest::Status)
            .await
            .is_ok()
        {
            return Ok(user);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "{} started but never answered on its socket. Its log said:\n{}",
        node.state.name,
        log_tail(&node.state.dir)
    )
}

/// Mint a single-use invite on the founder, exactly as `invite create` does
/// — built and signed here, because only this machine holds the user key.
async fn mint_invite(socket: &Path, user: &Keypair) -> Result<String> {
    let resp = ipc::request(socket, user, IpcRequest::InviteBootstrap).await?;
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = resp
    else {
        bail!("unexpected response to invite bootstrap");
    };
    let params = membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags: Vec::new(),
        bootstrap: addrs,
    };
    let (token, public) = membership::build_invite(mesh_id, user, &params, node_id)?;
    match ipc::request(socket, user, IpcRequest::InvitePublish { public }).await? {
        IpcResponse::Ok => Ok(token.encode()),
        IpcResponse::Error { message } => bail!("publishing the demo invite: {message}"),
        _ => bail!("unexpected response to invite publish"),
    }
}

/// Run `meshnode join` for one node, as a child, with a scrubbed
/// environment. A child rather than an in-process call so the join sees
/// exactly the config the daemon will see a moment later.
async fn join(exe: &Path, node: &NodeState, token: &str) -> Result<()> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--data-dir")
        .arg(&node.dir)
        .arg("join")
        .arg(token)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", node.port))
        .stdin(Stdio::null());
    scrub_env(&mut cmd);
    let out = cmd
        .output()
        .await
        .with_context(|| format!("joining {} to the demo mesh", node.name))?;
    if !out.status.success() {
        bail!(
            "{} could not join: {}",
            node.name,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Every demo node speaks as the founder *user*.
///
/// `join` admits a *node*; it does not enrol a *person*. Left alone the CLI
/// on demo-2 would sign with demo-2's freshly minted user key, which no
/// member has ever heard of, and every call there would answer "unknown or
/// revoked user". In production a person enrols once and an admin approves
/// them (`meshnode enrol` / `users approve`); for a single-host demo the
/// honest shortcut is to hand every node the founder's identity — which is
/// also what makes `meshnode --data-dir <demo>/demo-2 top` work for
/// somebody poking at the thing afterwards. `mesh.sh` reached the same
/// conclusion for the same reason.
///
/// Written *before* the daemon starts, so it finds a key rather than
/// minting one.
fn lend_founder_identity(founder_dir: &Path, node_dir: &Path) -> Result<()> {
    let from = mesh_core::identity::cli_user_identity_path(founder_dir, None);
    let to = mesh_core::identity::cli_user_identity_path(node_dir, None);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(&from, &to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

/// `meshnode demo`.
pub async fn run(opts: Options) -> Result<i32> {
    if !cfg!(target_os = "linux") {
        bail!(
            "`meshnode demo` needs Linux, because a meshware node needs Linux (D10). On this \
             machine, run Director and join a mesh that has one, or use `meshnode adopt` to \
             set up a Linux box you already have SSH to."
        );
    }
    if !(2..=16).contains(&opts.nodes) {
        bail!(
            "--nodes takes 2 to 16. One node is not a mesh — that is `meshnode up` — and past \
             sixteen this stops demonstrating the product and starts demonstrating your \
             scheduler."
        );
    }

    let exe = std::env::current_exe().context("finding this binary")?;
    let root = scratch_root();

    // Before anything is built: clean up after whatever went wrong last
    // time. This is the path a `kill -9` skipped.
    let reaped = reap_previous(&root).await;
    if reaped > 0 {
        eprintln!("  cleaned up {reaped} node(s) left by an earlier demo");
    }
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| format!("clearing {}", root.display()))?;
    }
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    // Slots are divided, not multiplied: a demo that saturates the machine
    // it is demonstrating on is a demo of the wrong thing.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let slots = (cores / opts.nodes).max(1);

    let mut udp_taken = Vec::new();
    let mut tcp_taken = Vec::new();
    let mut planned = Vec::new();
    for i in 1..=opts.nodes {
        let name = format!("demo-{i}");
        let dir = root.join(&name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let port = free_port(MESH_PORT_BASE, true, &mut udp_taken)?;
        let http_port = free_port(HTTP_PORT_BASE, false, &mut tcp_taken)?;
        std::fs::write(
            dir.join("meshnode.toml"),
            config_toml(&name, port, http_port, slots),
        )
        .with_context(|| format!("writing {}'s config", name))?;
        planned.push(NodeState {
            name,
            dir,
            port,
            http_port,
            pid: 0,
        });
    }

    let mut state = State {
        mesh_name: MESH_NAME.to_string(),
        started_at_micros: mesh_proto::now_micros(),
        nodes: Vec::new(),
    };

    eprintln!();
    eprintln!(
        "  Founding a {}-node mesh on this machine. Nothing outside {} is touched.",
        opts.nodes,
        root.display()
    );
    eprintln!();

    // A guard from here on: every early return past this point has to take
    // the children with it, and `Demo`'s drop is what guarantees that.
    let mut demo = Demo {
        root: root.clone(),
        nodes: Vec::new(),
        started: Instant::now(),
    };

    // The founder.
    step("demo-1  founding…");
    let child = spawn_node(&exe, &planned[0])?;
    let mut node = Node {
        state: planned[0].clone(),
        child,
    };
    node.state.pid = node.child.id().unwrap_or(0);
    state.nodes.push(node.state.clone());
    write_state(&root, &state)?;
    let founder_user = wait_ready(&mut node, Duration::from_secs(60)).await?;
    let founder_dir = node.state.dir.clone();
    let founder_socket = node.socket();
    demo.nodes.push(node);
    step_done(&format!("demo-1  up on 127.0.0.1:{}", planned[0].port));

    // …and everyone else.
    for planned_node in planned.iter().skip(1) {
        step(&format!("{}  joining…", planned_node.name));
        let token = mint_invite(&founder_socket, &founder_user).await?;
        join(&exe, planned_node, &token).await?;
        lend_founder_identity(&founder_dir, &planned_node.dir)?;
        let child = spawn_node(&exe, planned_node)?;
        let mut node = Node {
            state: planned_node.clone(),
            child,
        };
        node.state.pid = node.child.id().unwrap_or(0);
        state.nodes.push(node.state.clone());
        write_state(&root, &state)?;
        wait_ready(&mut node, Duration::from_secs(60)).await?;
        demo.nodes.push(node);
        step_done(&format!(
            "{}  up on 127.0.0.1:{}",
            planned_node.name, planned_node.port
        ));
    }

    // Membership converges over gossip, so "started" is not "in the mesh".
    // Waiting here is what makes the fan-out below hit every node instead
    // of whichever ones happened to have arrived.
    let converged = wait_for_members(&founder_socket, &founder_user, opts.nodes).await?;
    if !converged {
        eprintln!(
            "  note: only some nodes have appeared in the membership view yet — \
             the fan-out below will reach the ones that have."
        );
    }

    eprintln!();
    eprintln!("  Running `{PROOF}` on every machine in the mesh:");
    eprintln!();
    let mut stream = ipc::request_stream(
        &founder_socket,
        &founder_user,
        IpcRequest::Run {
            command: PROOF.to_string(),
            on: None,
            env: Vec::new(),
            timeout_secs: None,
            fanout: Some(0),
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await
    .context("submitting the demo fan-out")?;
    crate::consume_job_stream_raced(&mut stream, true, false, false).await?;

    print_summary(&demo, &root, cores, slots, demo.started.elapsed());

    wait_for_the_end(&mut demo).await;
    demo.teardown().await;
    Ok(0)
}

/// Poll the founder's membership view until every node has arrived.
async fn wait_for_members(socket: &Path, user: &Keypair, want: usize) -> Result<bool> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if let Ok(IpcResponse::Nodes(rows)) = ipc::request(socket, user, IpcRequest::Nodes).await {
            let live = rows
                .iter()
                .filter(|r| !r.left && !r.revoked && r.liveness != mesh_proto::Liveness::Dead)
                .count();
            if live >= want {
                return Ok(true);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}

/// Sit here until the person stops the demo, or a node falls over.
async fn wait_for_the_end(demo: &mut Demo) {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    loop {
        let died = tokio::time::sleep(Duration::from_millis(500));
        #[cfg(unix)]
        let terminated = async {
            match term.as_mut() {
                Some(t) => {
                    t.recv().await;
                }
                None => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let terminated = std::future::pending::<()>();

        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            () = terminated => return,
            () = died => {
                for node in &mut demo.nodes {
                    if let Ok(Some(status)) = node.child.try_wait() {
                        eprintln!();
                        eprintln!("  {} exited on its own ({status}) — stopping the demo.", node.state.name);
                        return;
                    }
                }
            }
        }
    }
}

/// The demo, and the promise that it leaves nothing behind.
struct Demo {
    root: PathBuf,
    nodes: Vec<Node>,
    started: Instant,
}

impl Demo {
    /// Stop politely, then insist, then remove the evidence.
    async fn teardown(&mut self) {
        eprintln!();
        eprintln!("  Stopping the demo…");
        for node in &mut self.nodes {
            #[cfg(unix)]
            signal(node.state.pid, rustix::process::Signal::TERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for node in &mut self.nodes {
            loop {
                match node.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = node.child.start_kill();
                        break;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        }
        for node in &mut self.nodes {
            let _ = node.child.wait().await;
        }
        match std::fs::remove_dir_all(&self.root) {
            // Already gone is the *other* success: `meshnode demo --stop`
            // from a second shell removes the root and then this teardown
            // runs. Reporting that as a failure would teach people that the
            // documented way to stop a demo does not work.
            Ok(()) | Err(_) if !self.root.exists() => {
                eprintln!("  Stopped. {} is gone.", self.root.display());
            }
            Err(e) => eprintln!(
                "  Stopped, but {} could not be removed ({e}) — `rm -rf` it when convenient.",
                self.root.display()
            ),
            Ok(()) => eprintln!("  Stopped. {} is gone.", self.root.display()),
        }
    }
}

/// What was built, what to try next, and what it costs to walk away.
///
/// The third of those is the one that gets skipped and the one that makes
/// the rest usable: a person will not run commands against a thing they are
/// not sure how to get rid of.
///
/// The core count is deliberately **not** `meshnode totals`. On a real mesh
/// that figure is a sum over machines and is true; here every node is the
/// same machine, so summing it would announce 144 cores on a 48-core box.
/// A demo that opens by overstating the hardware has spent the credibility
/// it exists to earn, so this says what is actually there and who is
/// sharing it.
fn print_summary(demo: &Demo, root: &Path, cores: usize, slots: usize, took: Duration) {
    let founder = &demo.nodes[0].state.dir;
    let n = demo.nodes.len();
    eprintln!();
    eprintln!(
        "  A {n}-machine mesh, founded and proven in {:.1}s.",
        took.as_secs_f64()
    );
    eprintln!();
    eprintln!("    mesh        {MESH_NAME}");
    for (i, node) in demo.nodes.iter().enumerate() {
        let label = if i == 0 { "machines" } else { "" };
        eprintln!(
            "    {label:<11} {:<8} 127.0.0.1:{}",
            node.state.name, node.state.port
        );
    }
    eprintln!("    cores       {cores} on this machine, shared by all {n} nodes");
    eprintln!(
        "    slots       {} — what the mesh runs at once ({slots} per node)",
        slots * n
    );
    eprintln!("    data        {}", root.display());
    eprintln!();
    eprintln!("  Point a shell at the first machine and it behaves like any node:");
    eprintln!();
    eprintln!("    export MESHWARE_HOME={}", founder.display());
    eprintln!("    meshnode top");
    eprintln!("    meshnode run 'sleep 2; hostname' --fanout all");
    eprintln!();
    eprintln!(
        "  Ctrl-C stops the demo and removes {}. Nothing else on this machine",
        root.display()
    );
    eprintln!("  was touched: no ~/.meshware, and nothing announced on your network.");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_demo_never_lands_in_the_real_home() {
        let root = scratch_root();
        let home = mesh_core::layout::default_home();
        assert_ne!(root, home);
        assert!(
            !root.starts_with(&home),
            "the demo root {root:?} must not live under {home:?} (D-L1.2)"
        );
    }

    #[test]
    fn the_config_is_loopback_only_and_announces_nothing() {
        let toml = config_toml("demo-1", 7901, 7951, 4);
        let cfg: mesh_core::config::NodeConfig = {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join("meshnode.toml"), &toml).expect("write");
            mesh_core::config::NodeConfig::load(
                Some(&dir.path().join("meshnode.toml")),
                &mesh_core::config::Overrides {
                    data_dir: Some(dir.path().to_path_buf()),
                    ..Default::default()
                },
            )
            .expect("the demo config must be a valid config")
        };
        assert!(
            cfg.listen.ip().is_loopback(),
            "listen {} is not loopback",
            cfg.listen
        );
        assert!(
            cfg.wan.advertise.is_empty(),
            "a demo node must advertise no WAN candidate (D-L1.2)"
        );
        assert!(!cfg.wan.relay, "a demo node must not act as a relay");
        assert_eq!(cfg.mesh_name, MESH_NAME);
        let http = cfg.http_addr.expect("a demo node keeps /healthz");
        assert!(http.ip().is_loopback());
        assert_eq!(
            http.port(),
            7951,
            "each node needs its own observability port"
        );
    }

    #[test]
    fn each_node_gets_its_own_ports() {
        let mut udp = Vec::new();
        let mut tcp = Vec::new();
        let ports: Vec<(u16, u16)> = (0..4)
            .map(|_| {
                (
                    free_port(MESH_PORT_BASE, true, &mut udp).expect("a free udp port"),
                    free_port(HTTP_PORT_BASE, false, &mut tcp).expect("a free tcp port"),
                )
            })
            .collect();
        let mut mesh: Vec<u16> = ports.iter().map(|(m, _)| *m).collect();
        mesh.sort_unstable();
        mesh.dedup();
        assert_eq!(mesh.len(), 4, "two demo nodes would fight over one port");
        let mut http: Vec<u16> = ports.iter().map(|(_, h)| *h).collect();
        http.sort_unstable();
        http.dedup();
        assert_eq!(http.len(), 4, "only the first node would bind /healthz");
    }

    #[test]
    fn a_recycled_pid_is_never_killed() {
        // This process is not a demo node, and its own pid must not be
        // mistaken for one however the state file got written.
        let root = scratch_root();
        assert!(!is_ours(std::process::id(), &root));
        assert!(!is_ours(1, &root), "pid 1 is never a demo node");
    }
}
