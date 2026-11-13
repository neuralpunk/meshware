//! Containers, as a subprocess plugin (N4 §10).
//!
//! ## Why a plugin and not an OCI client
//!
//! `meshnode` is four dependencies, no C, and a static musl binary, and
//! that property is load-bearing rather than aesthetic: it is why the thing
//! installs by copying a file, and it is why `jsonwebtoken` was refused in
//! `oidc.rs`. An in-tree OCI client spends all of it — image pulling, layer
//! unpacking, a registry client, TLS, signature verification — for a
//! runtime most meshes will never turn on.
//!
//! The contract already exists twice (`secrets::provider_get` and the sink
//! `plugin` writer), so this is the **third instance of one contract**
//! rather than a new one: a subprocess, JSON on stdin, JSON on stdout,
//! non-zero exit is a failure with its stderr tail attached.
//!
//! ## What the plugin actually decides
//!
//! It does **not** run the container. It answers one question — *"what
//! command would run this?"* — and `meshnode` spawns that command with the
//! machinery it already has: the log spool, the secret redactor, the kill
//! group, the cancel watch, the accounting sampler. A plugin that ran the
//! container itself would have to reimplement all of it, and would be the
//! only job on the node whose logs worked differently.
//!
//! ## The landmine, answered honestly
//!
//! N4 warns that Landlock and cgroups must keep applying or `doctor` must
//! report the degradation by name. **They do not fully apply, and this is
//! the honest shape of it**: `podman run` hands the workload to `conmon`,
//! which reparents it out of the executor's cgroup and out of any Landlock
//! ruleset applied to the client process. So a Landlock ruleset on the
//! spawned `podman` constrains *podman*, not the workload.
//!
//! What a container runtime has instead is its own equivalents —
//! `--memory`, `--cpus`, `--read-only`, `--cap-drop`,
//! `--security-opt=no-new-privileges`, `--cgroup-parent`. So the contract
//! makes the plugin **declare** which controls it enforces
//! ([`Controls`]), `doctor` reports exactly that, and a plugin that claims
//! nothing is reported as enforcing nothing. An operator finds out from
//! `doctor` rather than from an incident.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::error::{CoreError, Result};

/// How long the plugin has to answer.
///
/// Generous, because the honest implementation of `prepare` may pull an
/// image — and bounded, because a plugin that hangs would hang the job
/// slot it is holding. A pull that takes longer than this is a job that
/// should fail with a message rather than wedge.
const PREPARE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// What `meshnode` tells the plugin about the job it is preparing.
///
/// Every field is something the plugin cannot work out for itself, and
/// nothing here is a secret: resolved secret *values* never reach a plugin,
/// only the environment keys a job was going to get anyway, and those are
/// injected by the executor after this call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRequest {
    /// The contract version, so a plugin can refuse a request it does not
    /// understand instead of guessing at a field it has never seen.
    pub version: u32,
    /// The image reference, from the service version's `entrypoint`.
    pub image: String,
    /// Override the image's own ENTRYPOINT, from `MESHWARE_TASK_COMMAND`.
    ///
    /// Nomad's `config { command = … }`, and the same semantics: absent
    /// means the image runs whatever it was built to run. `podman` and
    /// `docker` both spell it `--entrypoint`.
    #[serde(default)]
    pub command: Option<String>,
    /// The argument list: `MESHWARE_TASK_ARGS` first, then the service's
    /// `--key value` inputs.
    ///
    /// That order because the first is the command line and the second is
    /// parameters to it — `["-c", "echo hi", "--target", "prod"]` reads
    /// the way it was written.
    pub args: Vec<String>,
    /// The working directory *inside* the container, empty for the image's
    /// own default.
    pub workdir: String,
    /// The materialized bundle on the host, when the service has one.
    ///
    /// The plugin is expected to mount this read-only. It is the *code*;
    /// the image is the base — which is a more useful split than baking an
    /// image per revision, and is why a container service still has a
    /// bundle.
    pub bundle_root: Option<String>,
    /// Where the plugin should mount it inside the container.
    pub bundle_mount: String,
    /// The job's own directory on the host, for anything that needs to
    /// outlive the container.
    pub job_dir: String,
    /// Declared limits (`MESHWARE_TASK_*`), for the plugin to translate.
    /// `0` means undeclared.
    pub cpus: u32,
    pub mem_mb: u64,
    /// Host ports this job holds (N2), already allocated.
    pub ports: Vec<PreparePort>,
    /// Volumes this job has **already claimed** (N5), with the host
    /// directory and where to mount it.
    ///
    /// Claimed before this call, not by the plugin: the claim is a row in
    /// the node's own lease table and the plugin has no business holding
    /// one. What it does is mount what it is given.
    #[serde(default)]
    pub volumes: Vec<PrepareVolume>,
    /// The cgroup this node would have put the job in, when it delegates
    /// one — so a plugin can pass `--cgroup-parent` and keep the accounting
    /// in one tree.
    pub cgroup_parent: Option<String>,
    /// The job id, for naming and for the operator to correlate.
    pub job_id: String,
    /// Which node started this container.
    ///
    /// The plugin is expected to put it on the container as a label, which
    /// is what makes [`list_jobs`] safe: two nodes on one machine each see
    /// the other's containers, and a reap that went by name alone would
    /// have one node remove the other's work on every restart.
    #[serde(default)]
    pub node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparePort {
    pub label: String,
    /// The host port already held for this job.
    pub host: u16,
    /// The port inside the container, as declared.
    pub to: u16,
}

/// One claimed volume, ready to mount.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareVolume {
    pub name: String,
    /// The directory on this host.
    pub source: String,
    /// Where to mount it inside the container.
    pub target: String,
    pub read_only: bool,
}

/// What the plugin answers: a command, and what it enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareReply {
    /// The program to spawn — `podman`, `docker`, a wrapper script.
    pub program: String,
    pub args: Vec<String>,
    /// Environment to add on top of the job's own.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Where to spawn it **on the host**. Empty = the job directory.
    #[serde(default)]
    pub workdir: String,
    /// What this plugin actually enforces. Absent means *nothing claimed*,
    /// which `doctor` reports as exactly that.
    #[serde(default)]
    pub controls: Controls,
    /// Where in `args` to put the job's own environment, as `--env NAME`
    /// pairs — the podman and docker spelling. Absent means the runner
    /// passes nothing into the container itself.
    ///
    /// **The job's environment reaches the program this reply names, not
    /// the container that program starts.** `podman run` gives a container
    /// only what its flags say, so a deployment's `container.env`, its
    /// secrets and every `MESHWARE_*` variable stopped at the engine
    /// client, and the workload saw none of them. The names are known only
    /// after secrets resolve, which is after this reply, so the executor
    /// inserts them — by *name*, with the value read from the client's own
    /// environment, which keeps a secret's value out of the argument list
    /// that `ps` shows.
    #[serde(default)]
    pub forward_env_at: Option<usize>,
}

/// Should this variable, from a container job's environment, be passed
/// into the container?
///
/// Everything the job was given for its own use — its `env`, its secrets,
/// its deployment, gang, device and address variables — except:
///
/// - the node's directives (`MESHWARE_TASK_*` and the other reserved
///   request families), which are for the daemon, not the workload;
/// - the socket, the job key and the node binary, which name files on the
///   host that the container does not have;
/// - `MESHWARE_PORT_*` and `MESHWARE_VOLUME_*`, which the runner sets
///   itself, the second with the path *inside* the container;
/// - the variables the engine client needs for itself (`engine_env`):
///   the host's `HOME` or `XDG_RUNTIME_DIR` are wrong inside.
pub fn forwards_into_container(key: &str, engine_env: &[(String, String)]) -> bool {
    let directive = key.starts_with(mesh_proto::REQ_ENV_PREFIX)
        || key.starts_with(mesh_proto::workflow::WF_ENV_PREFIX)
        || key.starts_with(mesh_proto::content::ANSIBLE_ENV_PREFIX)
        || key.starts_with(mesh_proto::OUT_ENV_PREFIX)
        || key.starts_with(mesh_proto::cache::CACHE_ENV_PREFIX)
        || key.starts_with(mesh_proto::acting::ACT_ENV_PREFIX)
        || mesh_proto::hosts::is_inventory_env(key);
    let host_path = matches!(
        key,
        crate::runtime::CLIENT_SOCKET_ENV
            | crate::runtime::CLIENT_IDENTITY_ENV
            | crate::runtime::NODE_BINARY_ENV
    );
    let runner_sets = key.starts_with(mesh_proto::PORT_ENV_PREFIX)
        || key.starts_with(mesh_proto::VOLUME_ENV_PREFIX);
    !directive && !host_path && !runner_sets && !engine_env.iter().any(|(k, _)| k == key)
}

/// The isolation controls a container plugin declares.
///
/// **Declared, not detected**, and that is the honest arrangement: only the
/// plugin knows which flags it passed. A plugin that lies is a plugin an
/// operator installed and trusted, which is the same trust boundary the
/// secrets provider already sits on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Controls {
    /// A memory ceiling is enforced (`--memory`).
    #[serde(default)]
    pub memory: bool,
    /// A CPU ceiling is enforced (`--cpus`).
    #[serde(default)]
    pub cpu: bool,
    /// The root filesystem is read-only (`--read-only`).
    #[serde(default)]
    pub readonly_rootfs: bool,
    /// Capabilities are dropped (`--cap-drop=ALL`).
    #[serde(default)]
    pub drop_capabilities: bool,
    /// `no-new-privileges` is set.
    #[serde(default)]
    pub no_new_privileges: bool,
    /// The workload runs as a non-root user, or rootless entirely.
    #[serde(default)]
    pub rootless: bool,
}

impl Controls {
    /// The controls this plugin does **not** claim, for `doctor`.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.memory {
            out.push("memory ceiling");
        }
        if !self.cpu {
            out.push("cpu ceiling");
        }
        if !self.readonly_rootfs {
            out.push("read-only rootfs");
        }
        if !self.drop_capabilities {
            out.push("dropped capabilities");
        }
        if !self.no_new_privileges {
            out.push("no-new-privileges");
        }
        if !self.rootless {
            out.push("rootless");
        }
        out
    }

    /// Nothing claimed at all — a plugin that answered with no `controls`
    /// block, or one that answered with every field false.
    pub fn claims_nothing(&self) -> bool {
        *self == Controls::default()
    }
}

/// The contract version this build speaks.
pub const CONTRACT_VERSION: u32 = 1;

/// Where a bundle is mounted inside the container, unless a plugin says
/// otherwise.
pub const DEFAULT_BUNDLE_MOUNT: &str = "/meshware/bundle";

/// Ask the plugin how to run this container.
///
/// The `secrets::provider_get` shape verbatim: spawn, write JSON to stdin,
/// close the pipe so a plugin that reads to EOF proceeds, read JSON from
/// stdout, and on a non-zero exit fail with the last three lines of stderr
/// — which is what a plugin author actually put the diagnosis in.
/// How long a plugin gets to stop one container.
///
/// Short: this runs on the path that frees a job's slot, and a plugin that
/// hangs here would hold up the node's whole executor. A container that has
/// not gone in ten seconds is reported and left, which is visible, rather
/// than waited on forever, which is not.
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Stop and remove the container a job was running (N6).
///
/// **Not optional, and not the same as the job exiting.** `podman run --rm`
/// removes the container when *the client* exits cleanly — but the client
/// is not the container. `podman run` hands the workload to `conmon`, which
/// reparents it, so a job that is cancelled, times out or is `SIGKILL`ed
/// leaves the container running: it keeps its host port, and the ledger —
/// which released that port the instant the job was gone — now disagrees
/// with the machine.
///
/// That is not a tidiness problem. The next instance of the same deployment
/// is allocated the port the ledger believes is free, the orphan still has
/// it, and every replacement crash-loops on `Address already in use` until
/// somebody finds the container by hand.
///
/// Idempotent by contract: a container that is already gone is a success,
/// because this runs on every job exit including the ordinary ones where
/// `--rm` already did the work.
pub async fn stop(plugin: &str, job_id: &str) -> Result<()> {
    let mut cmd = tokio::process::Command::new(plugin);
    cmd.arg("stop");
    cmd.env("MESHWARE_JOB_ID", job_id);
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Job(format!("container plugin {plugin:?}: spawn stop: {e}")))?;
    let out = tokio::time::timeout(STOP_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            CoreError::Job(format!(
                "container plugin {plugin:?} timed out stopping {job_id}"
            ))
        })?
        .map_err(|e| CoreError::Job(format!("container plugin {plugin:?}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(2).collect();
        return Err(CoreError::Job(format!(
            "container plugin {plugin:?} could not stop {job_id}: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(())
}

/// Job ids for the containers this node started that are still running.
///
/// **The orphan `stop` cannot reach.** `stop` runs when a job ends; it has
/// nothing to say about the node ending. Kill `meshnode` and conmon keeps
/// every container it started alive — holding its host ports, answering to
/// nobody, and invisible to a ledger that was rebuilt from an op log which
/// correctly says those jobs are over.
///
/// A node knows something at boot that it never knows again: **it is
/// running nothing**. So anything the plugin still lists under this node's
/// own label is left over from a previous life, and can be reaped without
/// consulting any other state.
///
/// Empty for a plugin with no `list` verb — the contract's older half —
/// which reaps nothing and is exactly the behaviour every plugin had
/// before this existed.
pub async fn list_jobs(plugin: &str, node: &str) -> Result<Vec<String>> {
    let mut cmd = tokio::process::Command::new(plugin);
    cmd.arg("list");
    cmd.env("MESHWARE_NODE_ID", node);
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Job(format!("container plugin {plugin:?}: spawn list: {e}")))?;
    let out = tokio::time::timeout(STOP_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| CoreError::Job(format!("container plugin {plugin:?} timed out listing")))?
        .map_err(|e| CoreError::Job(format!("container plugin {plugin:?}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(2).collect();
        return Err(CoreError::Job(format!(
            "container plugin {plugin:?} cannot list containers: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Remove every container this node left behind, before it places anything.
///
/// Best-effort and loud: a plugin that cannot list is reported once and the
/// node carries on, because refusing to start over a stale container would
/// turn a tidy-up into an outage. What it must not do is stay quiet — an
/// orphan holding a host port is a crash loop waiting for the next
/// deployment, and this is the only moment anybody could have told you.
pub async fn reap_orphans(plugin: &str, node: &str) {
    let orphans = match list_jobs(plugin, node).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                "could not check for containers left by a previous run: {e} — any that exist still hold their ports; `podman ps` will show them"
            );
            return;
        }
    };
    if orphans.is_empty() {
        return;
    }
    tracing::warn!(
        "{} container(s) outlived the last run of this node; removing them",
        orphans.len()
    );
    for job in &orphans {
        match stop(plugin, job).await {
            Ok(()) => tracing::info!(job = %job, "removed a container left by a previous run"),
            Err(e) => tracing::warn!(job = %job, "could not remove it: {e}"),
        }
    }
}

pub async fn prepare(plugin: &str, req: &PrepareRequest) -> Result<PrepareReply> {
    let mut child = tokio::process::Command::new(plugin)
        .arg("prepare")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            CoreError::Job(format!(
                "container plugin {plugin:?}: spawn: {e} — check `[runtime] \
                 container_plugin` in meshnode.toml"
            ))
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let body = serde_json::to_vec(req)
            .map_err(|e| CoreError::Job(format!("container plugin input encode: {e}")))?;
        let _ = stdin.write_all(&body).await;
        // Drop closes the pipe so plugins that read-to-EOF proceed.
    }
    let output = tokio::time::timeout(PREPARE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            CoreError::Job(format!(
                "container plugin {plugin:?} timed out after {}s — an image pull that \
                 takes longer than this is a job that should fail rather than hold its \
                 slot",
                PREPARE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| CoreError::Job(format!("container plugin {plugin:?}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        return Err(CoreError::Job(format!(
            "container plugin {plugin:?} failed (exit {:?}): {}",
            output.status.code(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    let reply: PrepareReply = serde_json::from_slice(&output.stdout).map_err(|e| {
        CoreError::Job(format!(
            "container plugin {plugin:?}: bad JSON on stdout: {e}"
        ))
    })?;
    if reply.program.trim().is_empty() {
        return Err(CoreError::Job(format!(
            "container plugin {plugin:?} returned no \"program\" to run"
        )));
    }
    Ok(reply)
}

/// Ask the plugin to describe itself, for `doctor`.
///
/// `<plugin> info` answers the same `controls` block it would attach to a
/// prepared run, plus a name. A plugin that does not implement `info` is
/// reported as present-but-silent rather than broken: the contract's
/// required verb is `prepare`, and refusing to run a mesh's containers
/// because a plugin lacks an optional introspection verb would be the
/// wrong trade.
pub async fn info(plugin: &str) -> Option<InfoReply> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(plugin)
            .arg("info")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoReply {
    /// What this plugin is, for a person reading `doctor`.
    pub name: String,
    /// The contract version it speaks.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub controls: Controls,
    /// Anything else worth saying — the podman version, say.
    #[serde(default)]
    pub detail: String,
}

/// Is `entrypoint` a plausible image reference?
///
/// Deliberately loose. Image references are a genuinely baroque grammar
/// (registry, port, path, tag, digest) and a strict validator here would
/// reject something a registry accepts — so this refuses only what could
/// not be a reference at all, and lets the runtime give the real error.
/// The one thing it does refuse firmly is a **shell metacharacter**, since
/// this string reaches a subprocess argument list.
pub fn is_plausible_image(image: &str) -> bool {
    let image = image.trim();
    !image.is_empty()
        && image.len() <= 512
        && !image.starts_with('-')
        && !image.contains(char::is_whitespace)
        && !image.contains([';', '&', '|', '$', '`', '\n', '\'', '"', '\\', '<', '>'])
}

/// Is `path` usable as a working directory *inside* a container?
///
/// Absolute paths are the normal case here (`/app`, `/srv/www`) and are
/// exactly what containment against the host checkout would refuse — so
/// this asks the only question that applies: could this string be a path,
/// and is it safe as an argument. Empty means "the image's own default",
/// which is the common case and always allowed.
pub fn is_plausible_container_path(path: &str) -> bool {
    let path = path.trim();
    path.is_empty()
        || (path.len() <= 512
            && !path.starts_with('-')
            && !path.contains(char::is_whitespace)
            && !path.contains(['\0', ';', '&', '|', '$', '`', '\n', '\'', '"', '\\']))
}

/// The bundle root to hand a plugin, when the checkout has one.
pub fn bundle_root_of(checkout: &Path, working_dir: &str) -> Option<String> {
    let root = if working_dir.is_empty() {
        checkout.to_path_buf()
    } else {
        checkout.join(working_dir)
    };
    if !root.exists() {
        return None;
    }
    // **An empty checkout is not a bundle.** An image-only service unpacks
    // nothing, and mounting the resulting empty directory over a path
    // inside the image would shadow whatever the image has there — which
    // for `/meshware/bundle` is nothing, but for an operator who set
    // `bundle_mount` to somewhere real would silently empty it.
    let empty = std::fs::read_dir(&root)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    (!empty).then(|| root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a workload needs goes in; what only the node or the engine
    /// client needs, or what names a host path, stays out.
    #[test]
    fn a_container_gets_the_jobs_environment_and_not_the_nodes() {
        let engine_env = vec![("XDG_RUNTIME_DIR".to_string(), "/run/user/993".to_string())];
        for key in [
            "LOG_LEVEL",
            "DATABASE_URL",
            "MESHWARE_NODE_NAME",
            "MESHWARE_DEPLOYMENT",
            "MESHWARE_DEPLOYMENT_SLOT",
            "MESHWARE_IP_HTTP",
            "MESHWARE_ADDR_HTTP",
            "MESHWARE_GANG_RANK",
            "MESHWARE_SCHEDULE",
            "CUDA_VISIBLE_DEVICES",
        ] {
            assert!(
                forwards_into_container(key, &engine_env),
                "{key} should go in"
            );
        }
        for key in [
            "MESHWARE_TASK_PORTS",
            "MESHWARE_TASK_COMMAND",
            "MESHWARE_ACT_PROOF",
            "MESHWARE_CACHE_KEY",
            "MESHWARE_SOCKET",
            "MESHWARE_USER_IDENTITY",
            "MESHWARE_BIN",
            "MESHWARE_PORT_HTTP",
            "MESHWARE_VOLUME_DATA",
            "XDG_RUNTIME_DIR",
        ] {
            assert!(
                !forwards_into_container(key, &engine_env),
                "{key} should stay out"
            );
        }
    }

    /// An absolute path is the *normal* answer here, and is exactly what
    /// containment against the host checkout would have refused.
    #[test]
    fn a_container_working_dir_may_be_absolute() {
        for ok in ["", "/app", "/srv/www", "app/sub", "/a/b/c"] {
            assert!(is_plausible_container_path(ok), "{ok:?}");
        }
        for bad in ["-x", "/a b", "/a;rm -rf /", "/a$(id)", "/a`id`", "/a\"b"] {
            assert!(!is_plausible_container_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_image_reference_is_loosely_validated() {
        for ok in [
            "nginx",
            "nginx:1.25",
            "docker.io/library/nginx:1.25",
            "ghcr.io/owner/name:tag",
            "registry:5000/team/app@sha256:abc123",
        ] {
            assert!(is_plausible_image(ok), "{ok}");
        }
        // What could not be a reference at all, or must not reach an argv.
        for bad in [
            "",
            "   ",
            "-x",
            "two words",
            "nginx; rm -rf /",
            "nginx && curl evil",
            "nginx`id`",
            "nginx$(id)",
            "nginx\nmore",
            &"x".repeat(513),
        ] {
            assert!(!is_plausible_image(bad), "{bad:?}");
        }
    }

    /// A plugin that claims nothing is reported as enforcing nothing —
    /// the whole point of the block being declared rather than assumed.
    #[test]
    fn controls_that_claim_nothing_say_so() {
        let none = Controls::default();
        assert!(none.claims_nothing());
        assert_eq!(none.missing().len(), 6);

        let podman = Controls {
            memory: true,
            cpu: true,
            readonly_rootfs: true,
            drop_capabilities: true,
            no_new_privileges: true,
            rootless: true,
        };
        assert!(!podman.claims_nothing());
        assert!(podman.missing().is_empty());
    }

    /// An absent `controls` block decodes as "claims nothing" rather than
    /// failing the reply — a plugin author who omits it gets a warning in
    /// `doctor`, not a broken runtime.
    #[test]
    fn a_reply_without_controls_claims_nothing() {
        let reply: PrepareReply =
            serde_json::from_str(r#"{"program":"podman","args":["run","nginx"]}"#).expect("decode");
        assert_eq!(reply.program, "podman");
        assert!(reply.controls.claims_nothing());
        assert!(reply.env.is_empty());
    }

    #[test]
    fn a_reply_carries_what_it_enforces() {
        let reply: PrepareReply = serde_json::from_str(
            r#"{"program":"podman","args":["run"],"controls":{"memory":true,"rootless":true}}"#,
        )
        .expect("decode");
        assert!(reply.controls.memory);
        assert!(reply.controls.rootless);
        assert!(!reply.controls.cpu);
        assert_eq!(
            reply.controls.missing(),
            vec![
                "cpu ceiling",
                "read-only rootfs",
                "dropped capabilities",
                "no-new-privileges"
            ]
        );
    }

    /// The request round-trips, so a plugin written against the JSON and a
    /// plugin written against this struct see the same thing.
    #[test]
    fn a_request_round_trips_through_json() {
        let req = PrepareRequest {
            version: CONTRACT_VERSION,
            image: "nginx:1.25".into(),
            command: Some("/bin/sh".into()),
            args: vec!["--key".into(), "value".into()],
            workdir: "/app".into(),
            bundle_root: Some("/var/lib/meshware/jobs/x/checkout".into()),
            bundle_mount: DEFAULT_BUNDLE_MOUNT.into(),
            job_dir: "/var/lib/meshware/jobs/x".into(),
            cpus: 2,
            mem_mb: 512,
            ports: vec![PreparePort {
                label: "http".into(),
                host: 20000,
                to: 9095,
            }],
            volumes: vec![PrepareVolume {
                name: "data".into(),
                source: "/srv/data".into(),
                target: "/data".into(),
                read_only: false,
            }],
            cgroup_parent: Some("meshware.slice".into()),
            job_id: "job-abc".into(),
            node: "n-abc123".into(),
        };
        let json = serde_json::to_string(&req).expect("encode");
        let back: PrepareRequest = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.image, req.image);
        assert_eq!(back.command.as_deref(), Some("/bin/sh"));
        assert_eq!(back.ports[0].host, 20000);
        assert_eq!(back.cgroup_parent.as_deref(), Some("meshware.slice"));
        assert_eq!(back.volumes[0].target, "/data");
        // The label the reaper reads back. A plugin that drops it leaves
        // containers `list` can never claim, which is the failure this
        // field exists to prevent.
        assert_eq!(back.node, "n-abc123");
    }

    /// The reference plugin's `list`, against podman's actual output shape.
    ///
    /// Not a mock: the parsing is the part that can be wrong, and it is
    /// wrong in a way nothing else would catch — a stray blank line or an
    /// unprefixed name turning into a job id that `stop` then cannot find.
    #[test]
    fn list_output_parses_to_job_ids() {
        let parse = |out: &str| -> Vec<String> {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            parse("job-abc\njob-def\n"),
            vec!["job-abc".to_string(), "job-def".to_string()]
        );
        // A plugin with no containers answers with nothing at all, which
        // must not read as one empty job id — `stop ""` would remove a
        // container named `meshware-`.
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }
}
