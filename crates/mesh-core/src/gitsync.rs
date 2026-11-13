//! Git sources (MASTERPLAN §8.1): Git as an optional *source*, never a
//! runtime dependency.
//!
//! A registered source is watched by one elected node (rendezvous hashing
//! over live members carrying the `git-sync` tag; all live executors when
//! none is tagged). The watcher clones/pulls on the source's interval,
//! ingests the working tree as an ordinary content-addressed bundle, and
//! auto-bumps the services named in the source's `bump` list onto the new
//! bundle. Git being down means staleness, never outage — executors keep
//! running whatever bundle replicated last.
//!
//! Sync shells out to the `git` binary (no libgit2 — static musl builds
//! stay C-free). A node without `git` simply fails the sync with a clear
//! error; another watcher can be tagged instead.
//!
//! **What a watcher fetches is rolled out to every service the source
//! bumps**, so how `git` runs is part of the mesh's security, not a detail
//! of it (SEC-21). It runs with an empty environment plus the handful of
//! variables a fetch needs, with no way to prompt, under a deadline, with
//! the transports that run programs or skip authentication switched off,
//! and — where the job sandbox is available — confined like a job: the
//! node's own directory out of reach, and nothing writable but its
//! checkout. Plain `http://` and `git://`, and unsigned commits, are the
//! node operator's call (`[git]`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tracing::{info, warn};

use crate::config::GitConfig;
use crate::content::ContentStore;
use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::scheduler;
use crate::services;
use crate::state::{MeshState, StateEngine};
use mesh_proto::{BundleId, GitSource, Liveness, NodeId, OpBody, ServiceVersion};

/// Rendezvous context for watcher election.
const ELECTION_CONTEXT: &[u8] = b"mw-gitsync-v1";

#[derive(Debug)]
pub struct SyncOutcome {
    pub commit: String,
    pub bundle_id: BundleId,
    /// True when the commit matched the last recorded sync — nothing new
    /// was ingested and no ops were authored.
    pub unchanged: bool,
    /// Services auto-bumped onto the new bundle, as (name, new version).
    pub bumped: Vec<(String, u32)>,
}

/// Build and sign a Git source record client-side (the CLI / Director holds
/// the user key, like invites and service versions).
pub fn build_source(
    user: &Keypair,
    name: String,
    url: String,
    branch: String,
    interval_secs: u64,
    bump: Vec<String>,
) -> GitSource {
    let mut source = GitSource {
        name,
        url,
        branch,
        interval_secs,
        bump,
        created_by: user.user_id(),
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    source.signature = user.sign(&source.signing_bytes()).to_vec();
    source
}

/// The node currently elected to watch `name`: rendezvous hashing over live
/// members tagged `git-sync`, falling back to all live executors. Every
/// node computes this from its own view; disagreement during churn at worst
/// double-syncs, and ingestion is content-addressed and idempotent.
pub fn elected_watcher(
    state: &MeshState,
    name: &str,
    liveness_of: impl Fn(&NodeId) -> Liveness,
) -> Option<NodeId> {
    let live: Vec<&mesh_proto::MemberInfo> = state
        .members
        .values()
        .filter(|m| state.is_active_member(&m.info.node_id))
        .filter(|m| matches!(liveness_of(&m.info.node_id), Liveness::Alive))
        .map(|m| &m.info)
        .collect();
    let tagged: Vec<NodeId> = live
        .iter()
        .filter(|m| m.tags.iter().any(|t| t == mesh_proto::GIT_SYNC_TAG))
        .map(|m| m.node_id)
        .collect();
    let pool = if tagged.is_empty() {
        live.iter()
            .filter(|m| !m.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG))
            .map(|m| m.node_id)
            .collect()
    } else {
        tagged
    };
    scheduler::rendezvous_rank(ELECTION_CONTEXT, name.as_bytes(), pool)
        .into_iter()
        .next()
}

/// How this node runs `git` (SEC-21): its `[git]` settings, and the job
/// sandbox policy git is confined under where that applies.
#[derive(Debug, Clone)]
pub struct GitRun {
    pub config: GitConfig,
    pub sandbox: crate::sandbox::Policy,
}

/// Would this node fetch `url`? The authoring door asks the same question
/// the watcher does, so the refusal comes with the command that caused it.
pub fn transport_refusal(config: &GitConfig, url: &str) -> Option<String> {
    (mesh_proto::is_plain_git_url(url) && !config.allow_plain).then(|| {
        format!(
            "{url:?} is fetched without authenticating the server or protecting the bytes, and \
             whatever comes back is rolled out to the services this source bumps — use https:// \
             or ssh://, or set `[git] allow_plain = true` on the nodes that watch it"
        )
    })
}

/// Run one sync of a source on this node: fetch, ingest, announce, record
/// status, bump tracked services. Safe to run concurrently with other
/// watchers' syncs of the same source (idempotent by content hash; version
/// bumps conflict-check at author time).
pub async fn sync_source(
    engine: &Arc<StateEngine>,
    content: &ContentStore,
    data_dir: &Path,
    source: &GitSource,
    run: &GitRun,
) -> Result<SyncOutcome> {
    // The last of the three checks (S5), and the one standing closest to
    // the thing it protects: this function passes `url` to `git` and turns
    // `name` into a directory, in the daemon process, outside every sandbox
    // a job runs in. The authoring path and the merge rule both refuse a
    // bad source first; a record written by an older build did not go
    // through either.
    source.validate().map_err(CoreError::Store)?;
    let root = data_dir.join("git-sources");
    let dir = root.join(&source.name);
    // Belt and braces on top of the name check: whatever the name is, the
    // directory it produced must still be directly under `git-sources/`.
    if dir.parent() != Some(root.as_path()) {
        return Err(CoreError::Store(format!(
            "git source {:?} does not name a directory under git-sources/",
            source.name
        )));
    }
    if let Some(why) = transport_refusal(&run.config, &source.url) {
        return Err(CoreError::Store(why));
    }
    // Scratch for git itself, beside the checkouts rather than in one: a
    // name holding `~` is not a source name, so it cannot collide.
    let tmp = root.join(".tmp~").join(&source.name);
    std::fs::create_dir_all(&tmp)?;
    // Before the rules name it: a Landlock rule for a missing path is an
    // error, and a fresh clone goes into this directory.
    let fresh = !dir.join(".git").exists();
    std::fs::create_dir_all(&dir)?;
    let git = Git::new(run, data_dir, &dir, &tmp)?;
    // Fetched without touching the working tree, verified if this node
    // asks for signed commits, and only then checked out — so a commit
    // that fails verification never reaches the tree that gets ingested.
    let fetched = if !fresh {
        git.run(
            &["fetch", "--depth", "1", "origin", &source.branch],
            Some(&dir),
        )
        .await?;
        "FETCH_HEAD"
    } else {
        let dir_str = dir.to_string_lossy().into_owned();
        git.run(
            &[
                "clone",
                "--no-checkout",
                "--depth",
                "1",
                "--branch",
                &source.branch,
                // Everything after this is a value, whatever it starts
                // with. The branch and url checks make it unnecessary; it
                // costs one argument to make it unnecessary *twice*.
                "--",
                &source.url,
                &dir_str,
            ],
            None,
        )
        .await?;
        "HEAD"
    };
    if run.config.require_signed {
        git.verify(fetched, &dir).await.map_err(|e| {
            CoreError::Store(format!(
                "git source {:?}: the fetched commit is not signed by a key this node trusts \
                 ([git] require_signed) — {e}",
                source.name
            ))
        })?;
    }
    git.run(&["reset", "--hard", fetched], Some(&dir)).await?;
    let commit = git
        .run(&["rev-parse", "HEAD"], Some(&dir))
        .await?
        .trim()
        .to_string();

    let last = engine.read(|s| {
        s.sources
            .get(&source.name)
            .and_then(|e| e.last.as_ref())
            .map(|l| (l.commit.clone(), l.bundle_id))
    });
    if let Some((last_commit, bundle_id)) = last
        && last_commit == commit
    {
        return Ok(SyncOutcome {
            commit,
            bundle_id,
            unchanged: true,
            bumped: Vec::new(),
        });
    }

    // Ingest the working tree (`.git` is skipped by the store) and announce
    // it exactly like `meshnode push` would.
    //
    // **Off the reactor** (**M-9**). `push_dir` walks the checkout with
    // `std::fs`, BLAKE3-hashes every file and writes them into the content
    // store — for a 200 MB repository that is one to five seconds with a
    // worker thread fully blocked, and gossip and job supervision share
    // that thread. It is already a pure function of a path and a store, so
    // there is nothing to untangle.
    let push = {
        let engine = engine.clone();
        let content = content.clone();
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || services::push_dir(&engine, &content, &dir))
            .await
            .map_err(|e| CoreError::State(format!("source ingest task failed: {e}")))??
    };
    engine.author_op(OpBody::SourceSynced {
        name: source.name.clone(),
        commit: commit.clone(),
        bundle_id: push.bundle_id,
    })?;
    info!(source = %source.name, commit = %&commit[..commit.len().min(12)],
        bundle = %push.bundle_id.short(), "git source synced");

    // Auto-bump tracked services onto the new bundle. The versions carry
    // `source = Some(name)` and an empty signature — authorized at merge
    // time through the source record (MASTERPLAN §8.1).
    let mut bumped = Vec::new();
    for svc in &source.bump {
        let Some(def) = engine.read(|s| s.service_current(svc).cloned()) else {
            continue; // service not registered (yet); nothing to bump
        };
        if def.bundle_id == push.bundle_id {
            continue;
        }
        let next = services::next_version(engine, svc);
        let version = ServiceVersion {
            name: svc.clone(),
            version: next,
            runtime: def.runtime,
            bundle_id: push.bundle_id,
            working_dir: def.working_dir,
            entrypoint: def.entrypoint,
            contract: def.contract,
            env: def.env,
            description: def.description,
            tags: def.tags,
            created_at_micros: mesh_proto::now_micros(),
            created_by: source.created_by,
            source: Some(source.name.clone()),
            signature: Vec::new(),
        };
        match engine
            .author_op(OpBody::ServiceVersionAdd { version })
            .and_then(|_| {
                engine.author_op(OpBody::ServiceHead {
                    name: svc.clone(),
                    version: next,
                    by: source.created_by,
                    signature: Vec::new(),
                })
            }) {
            Ok(_) => {
                info!(service = %svc, version = next, "auto-bumped from git source");
                bumped.push((svc.clone(), next));
            }
            Err(e) => warn!(service = %svc, "auto-bump failed: {e}"),
        }
    }

    Ok(SyncOutcome {
        commit,
        bundle_id: push.bundle_id,
        unchanged: false,
        bumped,
    })
}

/// Variables a fetch may need from the daemon's environment. Everything
/// else — including whatever a unit file or a shell exported into the
/// daemon — is left behind.
const PASS_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SSH_AUTH_SOCK",
    "GNUPGHOME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
];

/// Configuration every git command runs with. `ext::` and its siblings
/// run programs; hooks are the repository's own code; the filesystem
/// monitor is a program named in configuration.
const ALWAYS: &[&str] = &[
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "credential.interactive=false",
];

/// Added unless `[git] allow_plain`: a redirect or a submodule is refused
/// the transports the source itself is.
const NO_PLAIN: &[&str] = &[
    "-c",
    "protocol.http.allow=never",
    "-c",
    "protocol.git.allow=never",
];

/// One source's way of running git.
struct Git<'a> {
    run: &'a GitRun,
    /// The confinement, when the sandbox applies here.
    rules: Option<(PathBuf, crate::sandbox::Rules)>,
    env: Vec<(String, OsString)>,
}

impl<'a> Git<'a> {
    fn new(run: &'a GitRun, data_dir: &Path, checkout: &Path, tmp: &Path) -> Result<Git<'a>> {
        let mut env: Vec<(String, OsString)> = PASS_ENV
            .iter()
            .filter_map(|k| std::env::var_os(k).map(|v| (k.to_string(), v)))
            .collect();
        if !env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
        }
        let ssh =
            std::env::var_os("GIT_SSH_COMMAND").unwrap_or_else(|| "ssh -o BatchMode=yes".into());
        for (k, v) in [
            ("GIT_TERMINAL_PROMPT", OsString::from("0")),
            ("GIT_ASKPASS", OsString::from("/bin/false")),
            ("SSH_ASKPASS", OsString::from("/bin/false")),
            ("GIT_SSH_COMMAND", ssh),
            ("LC_ALL", OsString::from("C")),
            ("TMPDIR", tmp.as_os_str().to_os_string()),
        ] {
            env.push((k.to_string(), v));
        }
        let rules = match run.sandbox.mode {
            crate::sandbox::Mode::Off => None,
            mode => match crate::sandbox::unavailable_reason(&run.sandbox) {
                Some(why) if mode == crate::sandbox::Mode::On => {
                    return Err(CoreError::Store(format!(
                        "[runtime] sandbox = \"on\", but {why} — git is not run unconfined"
                    )));
                }
                Some(_) => None,
                None => crate::sandbox::shim_binary(&run.sandbox).map(|shim| {
                    let home = std::env::var_os("HOME").map(PathBuf::from);
                    let hidden = crate::sandbox::node_dirs(data_dir, home.as_deref());
                    let mut read = crate::sandbox::everything_but(&hidden);
                    read.extend([checkout.to_path_buf(), tmp.to_path_buf()]);
                    let mut write = crate::sandbox::system_write();
                    write.extend([checkout.to_path_buf(), tmp.to_path_buf()]);
                    (shim, crate::sandbox::Rules { read, write })
                }),
            },
        };
        Ok(Git { run, rules, env })
    }

    /// Run one git command, returning stdout; stderr becomes the error text.
    async fn run(&self, args: &[&str], cwd: Option<&Path>) -> Result<String> {
        self.run_with(args, cwd, &[]).await
    }

    /// `git verify-commit`, which needs to reach GnuPG's home and agent.
    async fn verify(&self, rev: &str, cwd: &Path) -> Result<String> {
        let mut extra = Vec::new();
        if let Some(home) = std::env::var_os("GNUPGHOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gnupg")))
        {
            extra.push(home);
        }
        if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR") {
            extra.push(PathBuf::from(run));
        }
        self.run_with(&["verify-commit", rev], Some(cwd), &extra)
            .await
    }

    async fn run_with(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        extra_write: &[PathBuf],
    ) -> Result<String> {
        let mut argv: Vec<OsString> = ALWAYS.iter().map(OsString::from).collect();
        if !self.run.config.allow_plain {
            argv.extend(NO_PLAIN.iter().map(OsString::from));
        }
        argv.extend(args.iter().map(OsString::from));
        let mut cmd = match &self.rules {
            Some((shim, rules)) => {
                let mut rules = rules.clone();
                rules
                    .write
                    .extend(extra_write.iter().filter(|p| p.exists()).cloned());
                rules.read.extend(rules.write.clone());
                let argv: Vec<String> = argv
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let mut cmd = tokio::process::Command::new(shim);
                cmd.args(rules.argv("git", &argv));
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new("git");
                cmd.args(&argv);
                cmd
            }
        };
        cmd.env_clear()
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // A group of its own, so a deadline stops what git started — an
        // ssh, a credential helper — and not only git.
        #[cfg(unix)]
        cmd.process_group(0);
        let what = args.first().unwrap_or(&"");
        let child = cmd
            .spawn()
            .map_err(|e| CoreError::Store(format!("git not runnable: {e}")))?;
        let pid = child.id();
        let deadline = self.run.config.timeout;
        let output = match tokio::time::timeout(deadline, child.wait_with_output()).await {
            Ok(output) => output.map_err(|e| CoreError::Store(format!("git not runnable: {e}")))?,
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = pid
                    .and_then(|p| i32::try_from(p).ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                #[cfg(not(unix))]
                let _ = pid;
                return Err(CoreError::Store(format!(
                    "git {what} did not finish within {} and was stopped ([git] timeout)",
                    humantime::format_duration(self.run.config.timeout)
                )));
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CoreError::Store(format!(
                "git {what} failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
