//! Director's fifth path — *Just show me* (L1 P3).
//!
//! The wizard's other four cards all assume the person already has a mesh,
//! or a second machine to make one with. This one assumes nothing: it
//! founds a small real mesh on the machine Director is running on, proves
//! it with a fan-out, and lands the app on a populated Mesh view about
//! forty seconds after the click.
//!
//! **It drives the same engine `meshnode demo` does** (`crates/meshware-node/src/demo.rs`,
//! which is the authority on everything here): real daemons, the real
//! scheduler, the real transport, loopback only, and nothing left behind.
//! Every machine is a `meshnode` process Director owns and reaps — the
//! founder, [`FOUNDER`], included — and Director is a view onto the founder,
//! exactly as it is onto this machine's own node the rest of the time.
//!
//! The one thing this file does that the CLI's does not is **move what
//! Director is looking at**. `engine::set_demo_root` points the whole
//! application at the demo — the founder's socket, and a scratch copy of
//! Director's own files — for the life of the demo, so every view command
//! follows without knowing a demo is happening, and `stop` puts it back. A
//! demo is ephemeral (D-L1.2), and the wizard has to be reachable again when
//! it ends.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{Manager, State};
use ts_rs::TS;

use mesh_core::membership;
use mesh_proto::{IpcRequest, IpcResponse};

use crate::engine::{self, Director};

/// What a demo mesh calls itself, on both frontends.
const MESH_NAME: &str = "demo";
/// The demo node that founds the mesh, and the one Director shows.
pub const FOUNDER: &str = "demo-1";
/// The fan-out that proves the mesh. One line per machine, nothing
/// installed, nothing changed.
const PROOF: &str = "uname -a";
/// Clear of the default 7777/7776 so a demo never contends with a real node.
const MESH_PORT_BASE: u16 = 7921;
const HTTP_PORT_BASE: u16 = 7971;

/// Progress, so a forty-second wait is not a spinner.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DemoEvent {
    /// One numbered step of the build.
    Step {
        message: String,
        done: u32,
        total: u32,
    },
    /// The mesh is up and has run something.
    Ready {
        mesh_name: String,
        machines: u32,
    },
    Failed {
        message: String,
    },
}

/// The demo Director is currently running.
pub struct DemoRun {
    pub root: PathBuf,
    /// The other machines. Dropping these kills them (`kill_on_drop`), and
    /// each was started with `up --supervised` so the kernel kills them too
    /// if Director is killed outright.
    pub children: Vec<tokio::process::Child>,
}

/// Where a Director demo lives. Never the real home, and never the same
/// directory a `meshnode demo` would use — the two must be able to run at
/// once without reaping each other.
pub fn scratch_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir.join("meshware-demo-director");
        }
    }
    std::env::temp_dir().join("meshware-demo-director")
}

/// A `meshnode` this machine can actually run, or nothing.
///
/// Same candidate order `deploy::from_local` uses, plus the binary Director
/// bundles for its own architecture. Nothing is downloaded: a demo that
/// needed the network would not be a demo of a product whose whole claim is
/// that it needs no cloud.
pub fn meshnode_binary(resource_dir: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = crate::localnode::bundle_dir(resource_dir) {
        candidates.push(dir.join(format!(
            "meshnode-{}-unknown-linux-musl",
            std::env::consts::ARCH
        )));
    }
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
    candidates.into_iter().find(|p| p.is_file())
}

/// Is the *Just show me* card worth offering on this machine?
///
/// Asked before the card is drawn rather than after it is clicked: a path
/// that fails when a stranger picks it is worse than a path that was never
/// shown. Nodes are Linux-only (D10), and the other machines need a
/// `meshnode` to be.
pub fn available(resource_dir: Option<&Path>) -> bool {
    meshnode_binary(resource_dir).is_some()
}

/// One demo node's `meshnode.toml`.
///
/// A near-copy of the CLI's, which is the authority (`meshware-node`'s
/// `demo::config_toml`). Duplicated rather than shared because Director
/// does not depend on the CLI crate. The `[http]` stanza is the
/// line neither copy may drop: the default observability address is a fixed
/// `127.0.0.1:7776`, so without one per node only the first binds it.
fn config_toml(name: &str, port: u16, http_port: u16, slots: usize) -> String {
    format!(
        "# Written by Director's demo. Everything under this directory is\n\
         # ephemeral: it is removed when the demo stops.\n\
         node_name = \"{name}\"\n\
         \n\
         [mesh]\n\
         name = \"{MESH_NAME}\"\n\
         \n\
         [listen]\n\
         addr = \"127.0.0.1\"\n\
         port = {port}\n\
         \n\
         [log]\n\
         level = \"warn\"\n\
         \n\
         [http]\n\
         addr = \"127.0.0.1:{http_port}\"\n\
         \n\
         [limits]\n\
         slots = {slots}\n\
         queue = 256\n\
         min_free_mem_mb = 0\n\
         \n\
         # Nothing resolves `.mesh` through a demo, and three nodes plus\n\
         # Director all reaching for 127.0.0.1:8600 is how a real meshnode\n\
         # started later ends up without its own (DIR-15).\n\
         [network]\n\
         dns = false\n"
    )
}

fn free_port(from: u16, udp: bool, taken: &mut Vec<u16>) -> Result<u16, String> {
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
    Err(format!("no free port near {from} on 127.0.0.1"))
}

/// Strip the operator's meshware environment out of a child's, so a shell
/// that had `MESHWARE_LISTEN` set cannot point a demo node at a real one.
fn scrub_env(cmd: &mut tokio::process::Command) {
    for var in [
        "MESHWARE_HOME",
        "MESHWARE_DATA_DIR",
        "MESHWARE_LISTEN",
        "MESHWARE_NODE_NAME",
        "MESHWARE_MESH_NAME",
        "MESHWARE_HTTP",
        "MESHWARE_API",
        "MESHWARE_TAGS",
        "MESHWARE_ADVERTISE",
        "MESHWARE_RELAY",
        "MESHWARE_USER_IDENTITY",
    ] {
        cmd.env_remove(var);
    }
}

/// Put this person's key in a demo node's home, the way Director once put
/// it in its own before founding (`mirror_user_for_bootstrap`).
///
/// **Copied as bytes**, so a sealed key stays sealed — writing out the
/// opened seed would leave a plaintext copy of a key somebody protected in
/// a scratch directory. The session unlock is carried across with it,
/// because the cache is keyed by the file's path and this is a new one:
/// without it the founder could not open the key it is founding with.
fn lend_user_key(home: &Path) -> Result<(), String> {
    let src = mesh_core::identity::default_user_identity_path();
    let dst = mesh_core::layout::Layout::new(home)
        .identity_dir()
        .join("user.key");
    let bytes = std::fs::read(&src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    mesh_core::keyfile::write_private(&dst, &bytes).map_err(|e| e.to_string())?;
    if let Some(kek) = mesh_core::keyfile::cache_get(&src) {
        // Best-effort: no `$XDG_RUNTIME_DIR` means no cache to write, and
        // an unsealed key needs none anyway.
        let _ = mesh_core::keyfile::cache_put(&dst, &kek);
    }
    Ok(())
}

/// Wait for a spawned node to have bootstrapped and to be answering.
///
/// Files before socket. The daemon writes its identity and genesis on first
/// run, and any call issued before that lands races it into minting a
/// second user key — after which every later call answers "unknown or
/// revoked user". `bench/harness/mesh.sh` documents the same trap.
async fn wait_ready(dir: &Path, child: &mut tokio::process::Child) -> Result<(), String> {
    let layout = mesh_core::layout::Layout::new(dir);
    let genesis = layout.mesh_dir().join("genesis.json");
    let user_key = mesh_core::identity::cli_user_identity_path(dir, None);
    let there = |p: &Path| p.metadata().map(|m| m.len() > 0).unwrap_or(false);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(60) {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("a demo node exited before it was ready ({status})"));
        }
        if there(&genesis) && there(&user_key) {
            let user = mesh_core::identity::load_cli_user(dir, None).map_err(|e| e.to_string())?;
            let socket = mesh_core::config::socket_path_for(dir);
            if mesh_core::ipc::request(&socket, &user, IpcRequest::Status)
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("a demo node started but never answered".into())
}

/// Kill whatever an earlier Director demo left behind, then clear the root.
///
/// A recorded pid is not trusted on its own: pids are recycled. Every
/// process signalled here is confirmed to be a meshnode running out of
/// *this* scratch root first.
fn reap(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let pid_file = entry.path().join("daemon.pid");
        let Ok(text) = std::fs::read_to_string(&pid_file) else {
            continue;
        };
        let Ok(pid) = text.trim().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let argv = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !(argv.contains("meshnode") && argv.contains(&*root.to_string_lossy())) {
            continue;
        }
        if let Ok(raw) = i32::try_from(pid)
            && let Some(pid) = rustix::process::Pid::from_raw(raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
}

/// The wizard's fifth path. Returns immediately; the work reports over the
/// channel and ends with `Ready` or `Failed`.
#[tauri::command]
pub async fn demo_start(
    app: tauri::AppHandle,
    state: State<'_, Director>,
    machines: u32,
    channel: Channel<DemoEvent>,
) -> Result<(), String> {
    if state.inner.lock().await.engine.is_some() {
        return Err("this Director is already in a mesh".into());
    }
    let machines = machines.clamp(2, 8);
    let resource_dir = app.path().resource_dir().ok();
    let Some(exe) = meshnode_binary(resource_dir.as_deref()) else {
        return Err(
            "Director could not find a meshnode binary to run the other machines with. \
             Install meshnode on this machine, or use one of the other ways in."
                .into(),
        );
    };

    tokio::spawn(async move {
        let director = app.state::<Director>();
        if let Err(message) = build(&director, &exe, machines, &channel).await {
            // Whatever got as far as running, stop. A half-built demo that
            // leaves daemons behind is the failure people remember.
            let _ = stop_inner(&director).await;
            let _ = channel.send(DemoEvent::Failed { message });
        }
    });
    Ok(())
}

async fn build(
    director: &State<'_, Director>,
    exe: &Path,
    machines: u32,
    channel: &Channel<DemoEvent>,
) -> Result<(), String> {
    let total = machines + 1;
    let mut done = 0u32;
    let step = |message: &str, done: &mut u32| {
        *done += 1;
        let _ = channel.send(DemoEvent::Step {
            message: message.to_string(),
            done: *done,
            total,
        });
    };

    let root = scratch_root();
    reap(&root);
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("clearing {}: {e}", root.display()))?;
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;

    // From here on the whole application looks at the demo.
    engine::set_demo_root(Some(root.clone()));
    director.inner.lock().await.demo = Some(DemoRun {
        root: root.clone(),
        children: Vec::new(),
    });

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let slots = (cores / total as usize).max(1);
    let mut udp = Vec::new();
    let mut tcp = Vec::new();
    let mut founder: Option<engine::Engine> = None;
    for i in 1..=total {
        let name = format!("demo-{i}");
        let dir = root.join(&name);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let port = free_port(MESH_PORT_BASE, true, &mut udp)?;
        let http_port = free_port(HTTP_PORT_BASE, false, &mut tcp)?;
        std::fs::write(
            dir.join("meshnode.toml"),
            config_toml(&name, port, http_port, slots),
        )
        .map_err(|e| e.to_string())?;
        // Every demo node speaks as this person: the founder founds the
        // mesh with their key, and `join` admits a node, not a person, so
        // without this the CLI on demo-2 would sign with a key nobody has
        // heard of. See the CLI's `demo::lend_founder_identity`.
        lend_user_key(&dir)?;

        if let Some(engine) = &founder {
            step(&format!("Adding {name}"), &mut done);
            let token = mint_invite(engine).await?;
            let mut join = tokio::process::Command::new(exe);
            join.arg("--data-dir")
                .arg(&dir)
                .arg("join")
                .arg(&token)
                .arg("--listen")
                .arg(format!("127.0.0.1:{port}"))
                .stdin(Stdio::null());
            scrub_env(&mut join);
            let out = join.output().await.map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(format!(
                    "{name} could not join: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        } else {
            step("Founding a mesh on this machine", &mut done);
        }

        let log = std::fs::File::create(dir.join("daemon.log")).map_err(|e| e.to_string())?;
        let mut up = tokio::process::Command::new(exe);
        up.arg("--data-dir")
            .arg(&dir)
            .arg("up")
            // The kernel kills this node when Director dies, which is the
            // one shutdown path no handler in this process can cover.
            .arg("--supervised")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        scrub_env(&mut up);
        let mut child = up.spawn().map_err(|e| format!("starting {name}: {e}"))?;
        if let Some(pid) = child.id() {
            let _ = std::fs::write(dir.join("daemon.pid"), pid.to_string());
        }
        wait_ready(&dir, &mut child).await?;
        director
            .inner
            .lock()
            .await
            .demo
            .as_mut()
            .ok_or("the demo was stopped while it was being built")?
            .children
            .push(child);

        if founder.is_none() {
            // Director's view of the demo is its founder, over its socket.
            let engine = engine::connect(None, false).await?;
            director.inner.lock().await.engine = Some(engine.clone());
            founder = Some(engine);
        }
    }
    let engine = founder.ok_or("the demo has no founder")?;

    // Membership converges over gossip, so "started" is not "in the mesh".
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if let Ok(IpcResponse::Nodes(rows)) = engine.ipc(IpcRequest::Nodes).await {
            let live = rows
                .iter()
                .filter(|r| !r.left && !r.revoked && r.liveness != mesh_proto::Liveness::Dead)
                .count();
            if live >= total as usize {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // One real fan-out, so the app opens with something in its history
    // rather than five empty states.
    let _ = engine
        .ipc(IpcRequest::Run {
            command: PROOF.to_string(),
            on: None,
            env: Vec::new(),
            timeout_secs: None,
            fanout: Some(0),
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        })
        .await;

    let _ = channel.send(DemoEvent::Ready {
        mesh_name: MESH_NAME.to_string(),
        machines: total,
    });
    Ok(())
}

async fn mint_invite(engine: &engine::Engine) -> Result<String, String> {
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = engine.ipc(IpcRequest::InviteBootstrap).await?
    else {
        return Err("unexpected response to invite bootstrap".into());
    };
    let params = membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags: Vec::new(),
        bootstrap: addrs,
    };
    let (token, public) = membership::build_invite(mesh_id, &engine.user, &params, node_id)
        .map_err(|e| e.to_string())?;
    match engine.ipc(IpcRequest::InvitePublish { public }).await? {
        IpcResponse::Ok => Ok(token.encode()),
        _ => Err("unexpected response to invite publish".into()),
    }
}

/// *Turn this into a real mesh* — and the app-exit path.
///
/// Stops every demo node, forgets the connection to the founder, removes
/// the scratch root and points Director back at this machine. Director is then exactly where it was before the
/// card was clicked: no mesh, and the wizard.
#[tauri::command]
pub async fn demo_stop(state: State<'_, Director>) -> Result<(), String> {
    stop_inner(&state).await
}

async fn stop_inner(state: &State<'_, Director>) -> Result<(), String> {
    let demo = {
        let mut inner = state.inner.lock().await;
        inner.engine = None;
        inner.demo.take()
    };
    if let Some(mut demo) = demo {
        for child in &mut demo.children {
            let _ = child.start_kill();
        }
        for child in &mut demo.children {
            let _ = child.wait().await;
        }
        if demo.root.exists() {
            let _ = std::fs::remove_dir_all(&demo.root);
        }
    }
    engine::set_demo_root(None);
    Ok(())
}

/// Called from the app's exit path: a demo must not outlive the window.
///
/// Deliberately **not** the full `stop`. The process is ending, and an exit
/// path that can block is an exit path that hangs — so this takes no lock, awaits nothing, and does
/// the two things that actually matter: signal the nodes and remove the
/// directory. The nodes were started with `up --supervised`, so the kernel
/// takes them even if this never runs at all.
pub fn stop_on_exit() {
    let root = scratch_root();
    if !root.exists() {
        return;
    }
    reap(&root);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_demo_never_lands_in_the_real_home() {
        let root = scratch_root();
        let home = mesh_core::layout::default_home();
        assert!(
            !root.starts_with(&home),
            "{root:?} must not live under {home:?}"
        );
        // …and never where a `meshnode demo` would put one, so the two can
        // run side by side without reaping each other's nodes.
        assert!(!root.ends_with("meshware-demo"));
    }

    #[test]
    fn the_demo_switch_moves_the_whole_app_and_puts_it_back() {
        let (node, app) = (engine::node_home(), engine::app_dir());
        let scratch = scratch_root();
        engine::set_demo_root(Some(scratch.clone()));
        assert_eq!(engine::node_home(), scratch.join(FOUNDER));
        assert_eq!(engine::app_dir(), scratch.join("director"));
        engine::set_demo_root(None);
        assert_eq!(engine::node_home(), node);
        assert_eq!(engine::app_dir(), app);
    }

    #[test]
    fn the_config_is_loopback_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        std::fs::write(&path, config_toml("demo-2", 7921, 7971, 4)).expect("write");
        let cfg = mesh_core::config::NodeConfig::load(
            Some(&path),
            &mesh_core::config::Overrides {
                data_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        )
        .expect("a demo config must be a valid config");
        assert!(cfg.listen.ip().is_loopback());
        assert!(cfg.wan.advertise.is_empty());
        assert_eq!(cfg.mesh_name, MESH_NAME);
        assert_eq!(cfg.http_addr.expect("healthz").port(), 7971);
        assert!(!cfg.network.dns, "a demo node binds no `.mesh` responder");
    }
}
