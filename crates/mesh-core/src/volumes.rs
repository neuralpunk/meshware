//! Dynamic volumes: provisioning through a plugin (N5 P2).
//!
//! P1's `[[volume]]` blocks are a statement an operator writes by hand
//! about directories that already exist. This is the other half — asking a
//! subprocess to *make* one — and it is Nomad's Dynamic Host Volume shape
//! on purpose: any executable, the operation in `$1`, inputs in the
//! environment, one JSON object on stdout. No gRPC, no sidecar, no
//! controller, no CRDs. `mesh_proto::volumes` holds the contract and the
//! argument for copying it, including the two holes this closes.
//!
//! **The created volume becomes an ordinary `[[volume]]`.** That is the
//! whole design. A plugin creates a directory and says where it is; from
//! that instant it is offered, claimed, leased, announced in `cap.volumes`
//! and released exactly as a static one, through the same code. There is no
//! second lifecycle for dynamic volumes, which is what keeps the claim and
//! the mount from becoming two facts that can disagree — the failure the
//! whole phase exists to avoid.
//!
//! **It is node-local and it is on disk.** The record lives in
//! `<data_dir>/volumes.json`, not the op-log: a volume *is* a directory on
//! one machine, so replicating a claim about it would be replicating
//! something no other node can verify or act on. The file is this node's
//! own note to itself about what it created, read back at boot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// How long a plugin gets to provision or remove one volume.
///
/// Generous, because a plugin may be carving an LVM volume or reaching a
/// storage appliance — and bounded, because an operator waiting on
/// `meshnode volume create` deserves an answer rather than a hang. A
/// plugin that needs longer than this is doing something that should be
/// asynchronous on its own side.
const PLUGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// One volume this node created, as remembered on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
    /// The plugin that made it — shown as the volume's `source`, so an
    /// operator reading the ledger can tell a hand-written block from a
    /// provisioned one without looking anywhere else.
    pub plugin: String,
    #[serde(default)]
    pub capacity_mb: u64,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub created_at_micros: u64,
    /// What the volume was asked for with, kept so `delete` can hand the
    /// plugin the same parameters `create` got. A plugin that needs to know
    /// which pool a volume came from to remove it has nowhere else to learn
    /// it, and asking the operator to retype it is how a volume gets
    /// orphaned.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// This node's record of what it has provisioned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub volumes: Vec<Created>,
}

/// How a plugin is named in the ledger: its file name.
fn source_of(plugin: &str) -> String {
    Path::new(plugin)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| plugin.to_string())
}

/// `<data_dir>/volumes.json`.
pub fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("volumes.json")
}

impl Store {
    /// Read the store, or an empty one.
    ///
    /// **A damaged file is empty, never fatal.** A node that refused to
    /// start because it could not parse its own note about four volumes
    /// would be a node that cannot run the other nine hundred jobs either.
    /// The directories are still there and still claimable the moment an
    /// operator writes a `[[volume]]` block for them.
    pub fn load(data_dir: &Path) -> Store {
        let path = store_path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Store::default();
        };
        match serde_json::from_slice::<Store>(&bytes) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(
                    "{}: {e} — this node's provisioned volumes are not offered until it is \
                     repaired or removed",
                    path.display()
                );
                Store::default()
            }
        }
    }

    /// Write it back, atomically: a half-written note about what exists on
    /// disk is worse than none.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = store_path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| CoreError::State(format!("encoding {}: {e}", path.display())))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::State(format!("{}: {e}", parent.display())))?;
        }
        std::fs::write(&tmp, &body)
            .map_err(|e| CoreError::State(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| CoreError::State(format!("{}: {e}", path.display())))
    }

    pub fn get(&self, name: &str) -> Option<&Created> {
        self.volumes.iter().find(|v| v.name == name)
    }

    /// The offered volumes this store contributes, in the shape
    /// `[[volume]]` produces — so everything downstream sees one list and
    /// cannot tell the two apart except by `source`.
    pub fn offered(&self) -> Vec<mesh_proto::Volume> {
        self.volumes
            .iter()
            .filter(|v| mesh_proto::volumes::is_valid_volume_name(&v.name))
            .filter(|v| Path::new(&v.path).is_absolute())
            .map(|v| mesh_proto::Volume {
                name: v.name.clone(),
                path: v.path.clone(),
                read_only: v.read_only,
                // The plugin's *name*, not the path it was invoked by.
                // `source` is a column in a table an operator reads, and
                // `/usr/local/libexec/meshware-mkdir` in a ten-character
                // field is how a table stops being one. The full path
                // stays in the store, which is what `delete` needs.
                source: source_of(&v.plugin),
            })
            .collect()
    }
}

/// Ask the plugin to provision `name`, and answer with what it made.
///
/// Every author-controlled value is validated **before it leaves**, so a
/// plugin author who forgets to quote `$MESHWARE_VOLUME_NAME` gets a broken
/// plugin, not a shell injection. Which plugin runs is never a request's
/// choice: `plugin` is the node's own `[runtime] volume_plugin`.
pub async fn create(
    plugin: &str,
    config: Option<&str>,
    name: &str,
    capacity_mb: u64,
    params: &BTreeMap<String, String>,
    want_path: Option<&str>,
) -> Result<mesh_proto::PluginCreated> {
    let mut cmd = tokio::process::Command::new(plugin);
    cmd.arg("create");
    apply_env(&mut cmd, config, name, capacity_mb, params, want_path)?;
    let out = run(plugin, "create", cmd).await?;
    let reply: mesh_proto::PluginCreated = serde_json::from_slice(&out).map_err(|e| {
        CoreError::State(format!(
            "volume plugin {plugin:?}: bad JSON on stdout for `create`: {e}"
        ))
    })?;
    let path = reply.path.trim();
    if !Path::new(path).is_absolute() {
        return Err(CoreError::State(format!(
            "volume plugin {plugin:?} answered with path {path:?}, which is not absolute — \
             meshware has to hand a job a real directory"
        )));
    }
    if !Path::new(path).is_dir() {
        return Err(CoreError::State(format!(
            "volume plugin {plugin:?} says it created {path:?}, and there is no directory \
             there — the plugin must clean up its own partial creates"
        )));
    }
    Ok(mesh_proto::PluginCreated {
        path: path.to_string(),
        ..reply
    })
}

/// Ask the plugin to remove `name`.
///
/// **Idempotent by contract**, and this side leans on it: a plugin that is
/// asked to delete something already gone must succeed. The alternative is
/// a volume that cannot be forgotten because the thing that would forget it
/// keeps failing.
pub async fn delete(plugin: &str, config: Option<&str>, vol: &Created) -> Result<()> {
    let mut cmd = tokio::process::Command::new(plugin);
    cmd.arg("delete");
    apply_env(
        &mut cmd,
        config,
        &vol.name,
        vol.capacity_mb,
        &vol.params,
        Some(&vol.path),
    )?;
    run(plugin, "delete", cmd).await.map(|_| ())
}

/// What a plugin says about itself. Optional, like the container plugin's
/// `info` — `doctor` prints it, and a plugin that answers nothing is
/// reported as unknown rather than as working.
pub async fn fingerprint(plugin: &str, config: Option<&str>) -> Result<String> {
    let mut cmd = tokio::process::Command::new(plugin);
    cmd.arg("fingerprint");
    cmd.env_clear();
    pass_through(&mut cmd);
    if let Some(config) = config {
        cmd.env(mesh_proto::PLUGIN_CONFIG_ENV, config);
    }
    let out = run(plugin, "fingerprint", cmd).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// The environment every operation gets, validated on the way out.
fn apply_env(
    cmd: &mut tokio::process::Command,
    config: Option<&str>,
    name: &str,
    capacity_mb: u64,
    params: &BTreeMap<String, String>,
    want_path: Option<&str>,
) -> Result<()> {
    if !mesh_proto::volumes::is_valid_volume_name(name) {
        return Err(CoreError::State(format!(
            "{name:?} is not a usable volume name: 1-48 letters, digits, '-' or '_', \
             starting with a letter or digit"
        )));
    }
    // **Every field, not just the name.** The parameters are the ones an
    // operator types and a plugin interpolates, so they are exactly the
    // ones worth refusing a metacharacter in.
    for (k, v) in params {
        for value in [k.as_str(), v.as_str()] {
            if !mesh_proto::volumes::is_safe_plugin_value(value) {
                return Err(CoreError::State(format!(
                    "volume parameter {value:?} contains a character meshware will not hand \
                     a plugin — shell metacharacters are refused here so a plugin that \
                     forgets to quote is merely broken"
                )));
            }
        }
    }
    if let Some(path) = want_path
        && !mesh_proto::volumes::is_safe_plugin_value(path)
    {
        return Err(CoreError::State(format!(
            "volume path {path:?} contains a character meshware will not hand a plugin"
        )));
    }
    // `env_clear`, like every other subprocess meshware spawns, plus the
    // few a plugin genuinely cannot work without.
    cmd.env_clear();
    pass_through(cmd);
    cmd.env(mesh_proto::PLUGIN_NAME_ENV, name);
    cmd.env(mesh_proto::PLUGIN_CAPACITY_ENV, capacity_mb.to_string());
    // JSON, for the same reason the device request is JSON: this is the
    // shape that will keep growing, and a second ad-hoc encoding is a
    // second parser to get wrong.
    cmd.env(
        mesh_proto::PLUGIN_PARAMS_ENV,
        serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string()),
    );
    if let Some(path) = want_path {
        cmd.env(mesh_proto::PLUGIN_PATH_ENV, path);
    }
    if let Some(config) = config {
        cmd.env(mesh_proto::PLUGIN_CONFIG_ENV, config);
    }
    Ok(())
}

/// The handful of variables a plugin's own runtime needs, for the reason
/// the container plugin's reply names them: only the plugin knows, and the
/// list of exceptions belongs beside the thing that wanted them.
fn pass_through(cmd: &mut tokio::process::Command) {
    for var in ["PATH", "HOME", "LANG", "XDG_RUNTIME_DIR"] {
        if let Some(v) = std::env::var_os(var) {
            cmd.env(var, v);
        }
    }
}

/// Run one operation, with the deadline and the failure shape the
/// container plugin already uses — one contract, three instances.
async fn run(plugin: &str, op: &str, mut cmd: tokio::process::Command) -> Result<Vec<u8>> {
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            CoreError::State(format!(
                "volume plugin {plugin:?}: spawn: {e} — check `[runtime] volume_plugin` in \
                 meshnode.toml"
            ))
        })?;
    let output = tokio::time::timeout(PLUGIN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            CoreError::State(format!(
                "volume plugin {plugin:?} timed out on `{op}` after {}s",
                PLUGIN_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| CoreError::State(format!("volume plugin {plugin:?}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        return Err(CoreError::State(format!(
            "volume plugin {plugin:?} failed on `{op}` (exit {:?}): {}",
            output.status.code(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(name: &str, path: &str) -> Created {
        Created {
            name: name.to_string(),
            path: path.to_string(),
            read_only: false,
            plugin: "test-plugin".to_string(),
            capacity_mb: 0,
            detail: String::new(),
            created_at_micros: 0,
            params: BTreeMap::new(),
        }
    }

    #[test]
    fn a_store_round_trips_and_a_damaged_one_is_empty() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();
        let store = Store {
            volumes: vec![created("data", "/srv/data"), created("cache", "/srv/cache")],
        };
        store.save(dir).expect("save");
        let back = Store::load(dir);
        assert_eq!(back.volumes, store.volumes);

        // Damaged is empty, not fatal: a node that would not start because
        // it could not parse its own note about two volumes is a node that
        // cannot run the other nine hundred jobs either.
        std::fs::write(store_path(dir), b"{ this is not json").expect("write");
        assert!(Store::load(dir).volumes.is_empty());

        // And a missing one is simply empty.
        std::fs::remove_file(store_path(dir)).expect("rm");
        assert!(Store::load(dir).volumes.is_empty());
    }

    /// What the store contributes is indistinguishable from a `[[volume]]`
    /// block except by `source`, which is the property that keeps there
    /// from being a second lifecycle for provisioned volumes.
    #[test]
    fn the_offered_list_is_ordinary_volumes() {
        let store = Store {
            volumes: vec![
                created("data", "/srv/data"),
                // Dropped rather than offered: a name no job can claim and
                // a path nothing can mount are not volumes.
                created("not a name", "/srv/x"),
                created("relative", "srv/y"),
            ],
        };
        let offered = store.offered();
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].name, "data");
        assert_eq!(offered[0].source, "test-plugin");
        // …and by file name, so the column stays a column.
        let long = Store {
            volumes: vec![Created {
                plugin: "/usr/local/libexec/meshware-mkdir".to_string(),
                ..created("d", "/srv/d")
            }],
        };
        assert_eq!(long.offered()[0].source, "meshware-mkdir");
        assert!(!offered[0].read_only);
    }

    #[tokio::test]
    async fn a_metacharacter_never_reaches_the_plugin() {
        let mut params = BTreeMap::new();
        params.insert("pool".to_string(), "a; rm -rf /".to_string());
        let err = create("/bin/true", None, "data", 0, &params, None)
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("shell metacharacters are refused"),
            "{err}"
        );

        // And the name itself, the field every plugin puts in a path.
        let err = create("/bin/true", None, "a;b", 0, &BTreeMap::new(), None)
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("not a usable volume name"),
            "{err}"
        );
    }

    /// A plugin that says it made something it did not is a failure, not a
    /// volume — otherwise the ledger advertises a directory that is not
    /// there and a job discovers it by failing to start.
    #[tokio::test]
    async fn a_plugin_that_lies_about_the_directory_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = tmp.path().join("liar");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"path\":\"/definitely/not/here\"}'\n",
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let err = create(
            script.to_str().expect("utf8"),
            None,
            "data",
            0,
            &BTreeMap::new(),
            None,
        )
        .await
        .expect_err("must refuse");
        assert!(err.to_string().contains("no directory there"), "{err}");
    }

    /// The happy path, end to end against a real subprocess: a plugin that
    /// makes a directory and says where it is.
    #[tokio::test]
    async fn a_plugin_that_creates_a_directory_is_believed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let target = tmp.path().join("made");
        let script = tmp.path().join("maker");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nmkdir -p '{}'\nprintf '{{\"path\":\"%s\",\"capacity_mb\":64}}' \
                 '{}'\n",
                target.display(),
                target.display()
            ),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let made = create(
            script.to_str().expect("utf8"),
            None,
            "data",
            64,
            &BTreeMap::new(),
            None,
        )
        .await
        .expect("create");
        assert_eq!(made.path, target.to_string_lossy());
        assert_eq!(made.capacity_mb, 64);
    }
}
