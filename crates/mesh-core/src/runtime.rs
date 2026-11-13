//! Runtime environment builder (MASTERPLAN §9.2, §11).
//!
//! Turns a materialized bundle + resolved service into a concrete command.
//! Python/Ansible get dependency-isolated environments cached by the content
//! hash of their requirements — so repeated runs skip the install. Bash/exec
//! run directly.
//!
//! Environment cache: `<data_dir>/envs/<runtime>-<reqhash>/`, with a `.ready`
//! marker written only after a successful build (a half-built env is never
//! reused). uv is preferred when present; otherwise `python -m venv` + pip.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mesh_proto::{InlineScript, MeshInventory, Runtime, ServiceExec};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{CoreError, Result};

/// A concrete plan for spawning a service's process.
#[derive(Debug)]
pub struct PreparedRun {
    pub program: String,
    pub args: Vec<String>,
    pub extra_env: Vec<(String, String)>,
    /// Directory to run in (the bundle's working_dir within `checkout`).
    pub workdir: PathBuf,
    /// For a container: where in `args` the job's environment goes, as
    /// `--env NAME` pairs (`PrepareReply::forward_env_at`).
    pub forward_env_at: Option<usize>,
}

fn reqs_hash(files: &[(&str, Vec<u8>)]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (name, bytes) in files {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes);
        hasher.update(b"\0");
    }
    data_encoding::HEXLOWER.encode(&hasher.finalize().as_bytes()[..16])
}

async fn tool_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Everything the container path needs from the node it is running on.
///
/// One struct rather than four positional parameters, because the four grew
/// one at a time until the non-container call sites read `(…, None, None,
/// "")` — three anonymous holes a reader has to count commas to interpret,
/// and that the compiler is happy to let you swap.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerContext<'a> {
    /// `[runtime] container_plugin`, when the operator configured one.
    ///
    /// **First in the resolution order** — an explicit override wins, which
    /// is the entire reason to have one. See [`prepare_container`].
    pub plugin: Option<&'a str>,
    /// The container engine this node detected for itself (T2).
    ///
    /// The ordinary path: a machine with podman or docker installed runs
    /// containers with no configuration anywhere.
    pub engine: Option<&'a crate::engines::Engine>,
    /// The cgroup this node would have put the job in, so the engine can
    /// keep the container in the same accounting tree.
    pub cgroup_parent: Option<&'a str>,
    /// This node's id, which becomes a label on the container. It is what
    /// lets a restarting node tell its own leftover containers from those
    /// of another node on the same machine — see `containers::list_jobs`.
    pub node: &'a str,
}

/// Prepare a run for the given service execution.
///
/// `job_env` is the spec's environment, *before* secret resolution. Only the
/// Ansible runtime reads it, and only for the reserved `MESHWARE_ANSIBLE_*`
/// flags the origin decided — see [`mesh_proto::ANSIBLE_ENV_PREFIX`].
pub async fn prepare(
    data_dir: &Path,
    checkout: &Path,
    exec: &ServiceExec,
    job_dir: &Path,
    job_env: &[(String, String)],
    container: ContainerContext<'_>,
) -> Result<PreparedRun> {
    // **Both stay inside the checkout** (S-L8). They come from a signed
    // `ServiceVersion` and both accepted `..`, so "the bundle root" was not
    // actually a boundary — which is not a privilege boundary on its own (a
    // `bash` service already runs as the daemon user) but is the invariant
    // the rest of the checkout story rests on, and it interacts badly with
    // the manifest paths S2 closed.
    // The containment check, not the *entry* check (S-L8). Both fields come
    // from a signed `ServiceVersion` and both accepted `..`, so the bundle
    // root was not actually a boundary — which is not a privilege boundary
    // on its own (a `bash` service already runs as the daemon user) but is
    // the invariant the rest of the checkout story rests on, and it
    // interacts badly with the manifest paths S2 closed.
    //
    // `check_contained_path` rather than `check_entry_path`: `.` is the
    // ordinary way to write "the checkout root" in a `working_dir` — the
    // SDK's `map` sends exactly that — and refusing it would be refusing a
    // correct value to enforce a rule about stored file names.
    // A container's `working_dir` is a path **inside the image** — it never
    // touches this host's filesystem, so containment against the checkout
    // is the wrong question and refuses the right answer (`/app`, `/srv`).
    // What it does need is to be safe as an argument, which
    // `is_plausible_container_path` checks.
    if exec.runtime == Runtime::Container {
        if !crate::containers::is_plausible_container_path(&exec.working_dir) {
            return Err(CoreError::Job(format!(
                "working_dir {:?} is not usable inside a container",
                exec.working_dir
            )));
        }
    } else {
        mesh_proto::content::check_contained_path("working_dir", &exec.working_dir)
            .map_err(CoreError::Job)?;
    }
    // **Except for a container, where `entrypoint` is an image reference.**
    // `docker.io/library/nginx:1.25` is not a path inside the bundle and
    // would fail containment for the wrong reason — it is checked by
    // `is_plausible_image` instead, which refuses the thing that actually
    // matters here: a shell metacharacter reaching an argument list.
    if exec.runtime != Runtime::Container {
        mesh_proto::content::check_contained_path("entrypoint", &exec.entrypoint)
            .map_err(CoreError::Job)?;
    } else if !crate::containers::is_plausible_image(&exec.entrypoint) {
        return Err(CoreError::Job(format!(
            "{:?} is not a usable image reference — for `runtime = \"container\"` the \
             entrypoint is the image, as in `docker.io/library/nginx:1.25`",
            exec.entrypoint
        )));
    }
    // A container's `working_dir` is a path *inside the image*, so it is
    // not looked for on the host — and an image may legitimately carry no
    // bundle at all.
    let workdir = if exec.runtime == Runtime::Container || exec.working_dir.is_empty() {
        checkout.to_path_buf()
    } else {
        checkout.join(&exec.working_dir)
    };
    if exec.runtime != Runtime::Container && !workdir.exists() {
        return Err(CoreError::Job(format!(
            "working_dir {:?} not found in bundle",
            exec.working_dir
        )));
    }
    if !matches!(exec.runtime, Runtime::Ansible | Runtime::Container) {
        let entry = workdir.join(&exec.entrypoint);
        if !entry.exists() {
            return Err(CoreError::Job(format!(
                "entrypoint {:?} not found in bundle working_dir",
                exec.entrypoint
            )));
        }
    }

    match exec.runtime {
        Runtime::Bash => Ok(PreparedRun {
            program: "bash".into(),
            args: std::iter::once(exec.entrypoint.clone())
                .chain(kv_args(&exec.inputs))
                .collect(),
            extra_env: Vec::new(),
            workdir,
            forward_env_at: None,
        }),
        Runtime::Exec => Ok(PreparedRun {
            program: format!("./{}", exec.entrypoint),
            args: kv_args(&exec.inputs),
            extra_env: Vec::new(),
            workdir,
            forward_env_at: None,
        }),
        Runtime::Python => prepare_python(data_dir, &workdir, exec).await,
        Runtime::Ansible => prepare_ansible(data_dir, &workdir, exec, job_dir, job_env).await,
        Runtime::Container => prepare_container(container, checkout, exec, job_dir, job_env).await,
    }
}

/// Work out the command that runs this image, and hand it back (N4, T2).
///
/// **`meshnode` spawns the command, not the engine or the plugin**, which is
/// what keeps a container job's logs, cancellation, kill group, secret
/// redaction and accounting identical to every other job's. Anything that
/// ran the container itself would have to reimplement all of it, and would
/// be the one job on the node whose logs worked differently.
///
/// ## The resolution order (T2 §4.5)
///
/// ```text
/// 1. [runtime] container_plugin is set  →  use it. An explicit override wins.
/// 2. otherwise                          →  the engine this node detected.
/// 3. neither                            →  refuse, naming what to install.
/// ```
///
/// Until T2 there was only step 1, and step 3 told people to go and write a
/// config line pointing at a bash script that existed in one git repository.
/// Step 2 is the ordinary path now: install podman or docker, start the
/// node, run containers — no file to copy, no line to write, and nothing to
/// write it on a thousand machines.
async fn prepare_container(
    container: ContainerContext<'_>,
    checkout: &Path,
    exec: &ServiceExec,
    job_dir: &Path,
    job_env: &[(String, String)],
) -> Result<PreparedRun> {
    let plugin = container.plugin.filter(|p| !p.trim().is_empty());
    if plugin.is_none() && container.engine.is_none() {
        return Err(CoreError::Job(
            concat!(
                "this node has no container engine, so it cannot run ",
                "`runtime = \"container\"`. Install podman or docker and restart ",
                "meshnode — it detects whichever is present and needs no further ",
                "configuration. `meshnode runtimes` shows what this node found. ",
                "Until then such jobs are placed on nodes that have one.",
            )
            .into(),
        ));
    }
    let reqs = mesh_proto::Requirements::from_env(job_env);
    let ports = mesh_proto::PortRequests::from_env(job_env);
    // The *allocated* ports, read back out of the environment this node
    // already wrote — so what the plugin publishes cannot disagree with
    // what the job was told.
    let allocated: Vec<crate::containers::PreparePort> = ports
        .0
        .iter()
        .filter_map(|r| {
            let key = format!("{}{}", mesh_proto::PORT_ENV_PREFIX, r.key_suffix());
            let host = job_env
                .iter()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v.parse::<u16>().ok())?;
            Some(crate::containers::PreparePort {
                label: r.label.clone(),
                host,
                to: if r.to > 0 { r.to } else { host },
            })
        })
        .collect();
    // The command override and its arguments come before the service's own
    // `--key value` inputs: the first is the command line, the second is
    // parameters to it.
    let (command, mut args) = mesh_proto::command_override(&exec.env);
    args.extend(kv_args(&exec.inputs));
    // The volumes this job **already holds** — read back out of the
    // environment the claim wrote, paired with the mount path the request
    // declared. The plugin mounts what it is given; it never claims.
    let claimed: Vec<crate::containers::PrepareVolume> =
        mesh_proto::VolumeRequests::from_env(job_env)
            .0
            .iter()
            .filter_map(|want| {
                let key = format!(
                    "{}{}",
                    mesh_proto::VOLUME_ENV_PREFIX,
                    mesh_proto::volumes::key_suffix(&want.name)
                );
                let source = job_env
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone())?;
                Some(crate::containers::PrepareVolume {
                    name: want.name.clone(),
                    source,
                    target: if want.path.trim().is_empty() {
                        format!("/meshware/volumes/{}", want.name)
                    } else {
                        want.path.clone()
                    },
                    read_only: want.mode.read_only(),
                })
            })
            .collect();
    let req = crate::containers::PrepareRequest {
        version: crate::containers::CONTRACT_VERSION,
        image: exec.entrypoint.clone(),
        command,
        args,
        workdir: exec.working_dir.clone(),
        bundle_root: crate::containers::bundle_root_of(checkout, ""),
        bundle_mount: crate::containers::DEFAULT_BUNDLE_MOUNT.to_string(),
        job_dir: job_dir.display().to_string(),
        cpus: reqs.cpus,
        mem_mb: reqs.mem_mb,
        ports: allocated,
        volumes: claimed,
        cgroup_parent: container.cgroup_parent.map(str::to_string),
        job_id: job_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        node: container.node.to_string(),
    };
    let reply = match plugin {
        Some(plugin) => crate::containers::prepare(plugin, &req).await?,
        // Unwrap is unreachable: the guard at the top returns when both are
        // absent, so one of the two arms always has something.
        None => container
            .engine
            .ok_or_else(|| CoreError::Job("no container engine".into()))?
            .plan(&req)?,
    };
    if reply.controls.claims_nothing() {
        // Said once, at the job, because `doctor` says it at the node and
        // an operator debugging *this* job should not have to go looking.
        warn!(
            runner = plugin.unwrap_or("built-in engine"),
            concat!(
                "container runner claims no isolation controls — the workload runs ",
                "with whatever the runtime's defaults are",
            )
        );
    }
    let workdir = if reply.workdir.trim().is_empty() {
        job_dir.to_path_buf()
    } else {
        PathBuf::from(&reply.workdir)
    };
    Ok(PreparedRun {
        program: reply.program,
        forward_env_at: reply.forward_env_at.filter(|&at| at <= reply.args.len()),
        args: reply.args,
        extra_env: reply.env,
        workdir,
    })
}

/// Prepare a run for a script carried inside the workflow (§4.7).
///
/// The trick is that there is no trick: the source is written into the
/// job's own work directory as a single file, and from there it is exactly
/// a one-file bundle. Python therefore gets the *same* cached, isolated
/// virtualenv a service would — keyed by the hash of the step's own
/// `requirements`, so two steps that need the same packages share one env
/// and a step with none pays nothing.
pub async fn prepare_inline(
    data_dir: &Path,
    workdir: &Path,
    script: &InlineScript,
) -> Result<PreparedRun> {
    std::fs::create_dir_all(workdir)?;
    let entry = script.entrypoint();
    std::fs::write(workdir.join(entry), script.source.as_bytes())?;

    match script.runtime {
        Runtime::Bash => Ok(PreparedRun {
            program: "bash".into(),
            args: std::iter::once(entry.to_string())
                .chain(kv_args(&script.inputs))
                .collect(),
            extra_env: Vec::new(),
            workdir: workdir.to_path_buf(),
            forward_env_at: None,
        }),
        Runtime::Python => {
            let reqs_path = workdir.join("requirements.txt");
            let reqs = script.requirements.trim();
            if !reqs.is_empty() {
                std::fs::write(&reqs_path, format!("{reqs}\n"))?;
            }
            let hash = reqs_hash(&[("requirements.txt", reqs.as_bytes().to_vec())]);
            let env_dir = data_dir.join("envs").join(format!("python-{hash}"));
            let python = build_python_env(&env_dir, &reqs_path, !reqs.is_empty()).await?;
            // Every Python step can `import meshware`, without anybody
            // having installed anything (see `build.rs`).
            Ok(PreparedRun {
                program: python.to_string_lossy().into_owned(),
                args: std::iter::once(entry.to_string())
                    .chain(kv_args(&script.inputs))
                    .collect(),
                extra_env: Vec::new(),
                workdir: workdir.to_path_buf(),
                forward_env_at: None,
            })
        }
        // Ansible needs a playbook tree and `exec` needs a binary; neither
        // is a thing you can type into a box. Parsing refuses these, so
        // reaching here means a definition was hand-edited past the
        // validator.
        other => Err(CoreError::Job(format!(
            "{} cannot run as an inline script — publish it to the Library",
            other.as_str()
        ))),
    }
}

/// key=value inputs → `--key value` argv pairs.
fn kv_args(inputs: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::with_capacity(inputs.len() * 2);
    for (k, v) in inputs {
        out.push(format!("--{k}"));
        out.push(v.clone());
    }
    out
}

async fn prepare_python(
    data_dir: &Path,
    workdir: &Path,
    exec: &ServiceExec,
) -> Result<PreparedRun> {
    let reqs_path = workdir.join("requirements.txt");
    let reqs = std::fs::read(&reqs_path).unwrap_or_default();
    let hash = reqs_hash(&[("requirements.txt", reqs.clone())]);
    let env_dir = data_dir.join("envs").join(format!("python-{hash}"));
    let python = build_python_env(&env_dir, &reqs_path, !reqs.is_empty()).await?;
    // Same for a Library service: a mapped function that wants to talk to
    // the mesh can, on any node, with nothing installed.

    Ok(PreparedRun {
        program: python.to_string_lossy().into_owned(),
        args: std::iter::once(exec.entrypoint.clone())
            .chain(kv_args(&exec.inputs))
            .collect(),
        extra_env: Vec::new(),
        workdir: workdir.to_path_buf(),
        forward_env_at: None,
    })
}

/// Build (or reuse) a venv and return its python interpreter path.
/// One build at a time per environment directory.
///
/// The environment is keyed by the *content hash of the requirements*, so
/// two tasks that need the same packages deliberately share one env — and
/// therefore race to build it. Both would then `remove_dir_all` the
/// directory the other is populating, and one of them fails with a bare
/// `File exists` from `venv`. That was invisible while parallel Python work
/// arrived one job at a time and is immediate the moment somebody maps a
/// hundred items over one script, which is exactly what C6 makes easy.
///
/// In-process only, and that is enough: the env dir lives under this node's
/// data dir and this node is one process. A second daemon on the same data
/// dir is already excluded by the store's own lock.
static PYTHON_ENV_LOCKS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn python_env_lock(env_dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    PYTHON_ENV_LOCKS
        .lock()
        .entry(env_dir.to_path_buf())
        .or_default()
        .clone()
}

/// Where the node that spawned a task is listening.
///
/// A task that talks back to the mesh it is running in has to find its own
/// daemon, and the job environment is cleared apart from a six-name
/// allowlist. Without this it fell back to the pointer file under
/// `$HOME/.meshware`, which is the executing node's daemon only by
/// coincidence — and on any node started with `--data-dir` or
/// `$MESHWARE_HOME`, the wrong daemon or none at all.
///
/// Deliberately **not** in `VOLATILE_ENV`: the executor sets this at spawn,
/// long after the action key was computed from the plan's env on the
/// origin, so it never reaches a key. A step that sets it *itself* has
/// changed what the task does and must change the key.
pub const CLIENT_SOCKET_ENV: &str = "MESHWARE_SOCKET";

/// The `meshnode` binary that is running this job, by absolute path.
///
/// A job that wants to invoke meshnode itself — `meshnode bench` on every
/// node is the motivating case — cannot rely on `PATH`. The executor clears
/// the environment and re-admits six variables from the *daemon's* own
/// environment, so `PATH` is whatever systemd, a shell profile, or a
/// hand-started daemon happened to have. On a node run from `/opt`, from a
/// build tree, or from a second binary alongside a packaged one, "meshnode"
/// on `PATH` is a different build from the one dispatching the work — or is
/// nothing at all.
///
/// `/proc/self/exe` is the only answer that is always right: it is *this*
/// daemon's binary, so a job invoking it gets the same code, the same
/// version and the same kernels as every other node running the same build.
/// That is what makes a fleet-wide benchmark comparable rather than
/// approximate.
///
/// Like [`CLIENT_SOCKET_ENV`], a default rather than an override, and
/// deliberately **not** in `VOLATILE_ENV`: the executor sets it at spawn,
/// after any action key was computed, so it never reaches a cache key.
pub const NODE_BINARY_ENV: &str = "MESHWARE_BIN";

/// The mesh's name for the node this job is running on.
///
/// Listed in `VOLATILE_ENV` since M9 as a variable the mesh sets per job —
/// and until now nothing set it. A job asking `$MESHWARE_NODE_NAME` got
/// nothing and fell back to the hostname, which is a different name and
/// occasionally the *same* name on two different machines.
///
/// The gap surfaced in the benchmark: a fan-out labels each result with the
/// mesh's name for the target, but a single-node step has no such wrapper,
/// so the control run identified itself by hostname while every fan-out
/// result identified itself by mesh name. The two could never be matched to
/// each other, and the contention figure that depends on pairing them read
/// as unknown on every run.
///
/// A default like [`CLIENT_SOCKET_ENV`], so a step that sets it for its own
/// reasons still wins.
pub const NODE_NAME_ENV: &str = "MESHWARE_NODE_NAME";

/// The user key a task signs with, defaulted to the executing node's own
/// `identity/user.key` — the same key the CLI on that machine signs with.
///
/// A default, not an override: it goes in before the service, runtime and
/// job env, so a step that points this at somebody else's key still wins.
pub const CLIENT_IDENTITY_ENV: &str = "MESHWARE_USER_IDENTITY";

async fn build_python_env(env_dir: &Path, reqs_path: &Path, has_reqs: bool) -> Result<PathBuf> {
    let python_bin = env_dir.join("bin").join("python");
    let ready = env_dir.join(".ready");
    if ready.exists() && python_bin.exists() {
        debug!(env = %env_dir.display(), "reusing python env");
        return Ok(python_bin);
    }
    // Serialize builders of *this* env, then look again: the task that was
    // ahead has almost certainly just built it, and rebuilding would throw
    // away a working environment somebody is about to use.
    let lock = python_env_lock(env_dir);
    let _guard = lock.lock().await;
    if ready.exists() && python_bin.exists() {
        debug!(env = %env_dir.display(), "reusing python env built while waiting");
        return Ok(python_bin);
    }
    // Clean any half-built env.
    if env_dir.exists() {
        let _ = std::fs::remove_dir_all(env_dir);
    }
    std::fs::create_dir_all(env_dir)?;

    let use_uv = tool_exists("uv").await;
    info!(env = %env_dir.display(), uv = use_uv, "building python env");

    if use_uv {
        run_checked(
            Command::new("uv").args(["venv", &env_dir.to_string_lossy()]),
            "uv venv",
        )
        .await?;
        if has_reqs {
            run_checked(
                Command::new("uv")
                    .args([
                        "pip",
                        "install",
                        "--python",
                        &python_bin.to_string_lossy(),
                        "-r",
                    ])
                    .arg(reqs_path),
                "uv pip install",
            )
            .await?;
        }
    } else {
        run_checked(
            Command::new("python3").args(["-m", "venv", &env_dir.to_string_lossy()]),
            "python -m venv",
        )
        .await?;
        if has_reqs {
            run_checked(
                Command::new(&python_bin)
                    .args([
                        "-m",
                        "pip",
                        "install",
                        "--disable-pip-version-check",
                        "-q",
                        "-r",
                    ])
                    .arg(reqs_path),
                "pip install",
            )
            .await?;
        }
    }
    std::fs::write(&ready, b"ok")?;
    Ok(python_bin)
}

async fn prepare_ansible(
    data_dir: &Path,
    workdir: &Path,
    exec: &ServiceExec,
    job_dir: &Path,
    job_env: &[(String, String)],
) -> Result<PreparedRun> {
    // Resolve ansible-playbook: from a requirements-built venv if the bundle
    // pins deps, else the system tool.
    let reqs_path = workdir.join("requirements.txt");
    let has_reqs = reqs_path.exists();
    let (program, extra_path) = if has_reqs {
        let reqs = std::fs::read(&reqs_path).unwrap_or_default();
        let hash = reqs_hash(&[("requirements.txt", reqs)]);
        let env_dir = data_dir.join("envs").join(format!("ansible-{hash}"));
        let python = build_python_env(&env_dir, &reqs_path, true).await?;
        // Ensure ansible is in the venv.
        let ap = env_dir.join("bin").join("ansible-playbook");
        if ap.exists() {
            (ap.to_string_lossy().into_owned(), Some(env_dir.join("bin")))
        } else {
            // venv without ansible pinned: fall back to system, but expose the
            // venv's python for modules.
            let _ = python;
            ("ansible-playbook".to_string(), Some(env_dir.join("bin")))
        }
    } else {
        if !tool_exists("ansible-playbook").await {
            return Err(CoreError::Job(
                "ansible-playbook not found on this node and the bundle pins no requirements"
                    .into(),
            ));
        }
        ("ansible-playbook".to_string(), None)
    };

    let mut args = vec![exec.entrypoint.clone()];

    // Inventory: mesh-rendered (the M2 stub below, set only by an origin
    // that predates the host inventory), bundle-provided, or implicit
    // localhost.
    if let Some(inv) = &exec.inventory {
        let inv_path = job_dir.join("mesh-inventory.ini");
        std::fs::write(&inv_path, render_inventory(inv))?;
        args.push("-i".into());
        args.push(inv_path.to_string_lossy().into_owned());
    }

    // The flags the origin decided (M10 §4.11). Reserved keys, so nothing a
    // step wrote by hand can widen a `--limit` an operator narrowed.
    let flag = |key: &str| {
        job_env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    if let Some(limit) = flag(mesh_proto::ANSIBLE_LIMIT) {
        args.push("--limit".into());
        args.push(limit.to_string());
    }
    if let Some(tags) = flag(mesh_proto::ANSIBLE_TAGS) {
        args.push("--tags".into());
        args.push(tags.to_string());
    }
    if flag(mesh_proto::ANSIBLE_CHECK) == Some("1") {
        args.push("--check".into());
    }
    // How hard this play may hit the estate (M25 §4.2). Clamped here as
    // well as at authoring time, because the value reaching an executor
    // has crossed a wire and the executor is the machine that would spawn
    // the processes.
    if let Some(forks) = flag(mesh_proto::ANSIBLE_FORKS).and_then(mesh_proto::ansible_forks) {
        args.push("--forks".into());
        args.push(forks.to_string());
    }

    // Inputs → extra-vars.
    for (k, v) in &exec.inputs {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }

    let mut extra_env = Vec::new();
    if let Some(bin) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        extra_env.push(("PATH".into(), format!("{}:{path}", bin.display())));
    }
    // Keep ansible quiet-ish and non-interactive.
    extra_env.push(("ANSIBLE_HOST_KEY_CHECKING".into(), "False".into()));
    extra_env.push(("ANSIBLE_RETRY_FILES_ENABLED".into(), "False".into()));

    Ok(PreparedRun {
        program,
        args,
        extra_env,
        workdir: workdir.to_path_buf(),
        forward_env_at: None,
    })
}

/// Render a mesh inventory as an Ansible INI. Hosts run with local connection
/// in M2 (all-on-one-host demos and self-targeting); SSH-based fan-out to
/// remote hosts is refined alongside credentials in M3.
fn render_inventory(inv: &MeshInventory) -> String {
    let mut out = String::from("[mesh]\n");
    for (name, ip) in &inv.hosts {
        out.push_str(&format!(
            "{name} ansible_host={ip} ansible_connection=local\n"
        ));
    }
    out
}

async fn run_checked(cmd: &mut Command, label: &str) -> Result<()> {
    let output = cmd
        .output()
        .await
        .map_err(|e| CoreError::Job(format!("{label}: spawn failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CoreError::Job(format!("{label} failed:\n{tail}")));
    }
    Ok(())
}
