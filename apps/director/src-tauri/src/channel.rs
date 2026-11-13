//! The release channel, from Director.
//!
//! Where a new meshnode comes from when nobody built one: the project's
//! signed releases. Three things happen here and each is a sentence on the
//! Updates screen — *is there a newer version*, *get it into the mesh*,
//! and *what would verify it*.
//!
//! The rules are the ones `meshnode upgrade --self` already lives by
//! (M7), restated for a desktop: a download is nothing until a key the
//! fleet trusts has signed for it; the placeholder key compiled into a
//! development build verifies nothing and says so; and the only test seam
//! is the same `MESHWARE_RELEASE_BASE` the CLI honours, loudly.

use std::path::Path;

use mesh_proto::{IpcRequest, IpcResponse};
use tauri::State;
use tauri::ipc::Channel;

use crate::engine::{Director, Engine};
use crate::view::{PublishedReleaseView, ReleaseCheckView, ReleaseFetchEvent};

/// Release repository.
pub const REPO: &str = "neuralpunk/meshware";
const DEFAULT_BASE: &str = "https://github.com/neuralpunk/meshware/releases/download";
/// Body cap for a binary download.
const MAX_BINARY: usize = 256 * 1024 * 1024;
/// The pinned release signing key — the same file `meshnode` embeds and
/// CI asserts `install.sh` carries.
pub(crate) const EMBEDDED_PUBKEY_FILE: &str = include_str!("../../../../packaging/minisign.pub");

/// `(base, custom)`: an explicit/env base is a test seam — plain `http://`
/// is tolerated there, and the log says so.
pub fn release_base() -> (String, bool) {
    match std::env::var("MESHWARE_RELEASE_BASE")
        .ok()
        .filter(|b| !b.trim().is_empty())
    {
        Some(b) => {
            let b = b.trim().trim_end_matches('/').to_string();
            if b.starts_with("http://") {
                tracing::warn!(
                    "release base {b} uses plain http — signatures are still verified, but this \
                     is a test seam"
                );
            }
            (b, true)
        }
        None => (DEFAULT_BASE.to_string(), false),
    }
}

/// The embedded key, unless it is the placeholder.
pub fn embedded_pubkey() -> Option<String> {
    let key = EMBEDDED_PUBKEY_FILE
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment"))?;
    if key.to_ascii_lowercase().contains("placeholder") {
        return None;
    }
    Some(key.to_string())
}

/// The keys a download is checked against: this node's own `[release]
/// pubkey` outranks everything; else the keys the mesh trusts; else the
/// env seam; else the embedded key. Empty means nothing would verify.
pub async fn trusted_keys(engine: &Engine) -> Vec<String> {
    if let Ok(IpcResponse::ReleaseKeys(v)) = engine.ipc(IpcRequest::ReleaseKeys).await {
        if let Some(local) = v.local_override {
            return vec![local];
        }
        let keys: Vec<String> = v
            .keys
            .into_iter()
            .filter(|k| !k.revoked)
            .map(|k| k.pubkey)
            .collect();
        if !keys.is_empty() {
            return keys;
        }
    }
    if let Some(k) = std::env::var("MESHWARE_MINISIGN_PUBKEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        return vec![k.trim().to_string()];
    }
    embedded_pubkey().into_iter().collect()
}

fn client() -> Result<mesh_core::httpc::Client, String> {
    let (base, custom) = release_base();
    mesh_core::httpc::Client::new(custom && base.starts_with("http://"), MAX_BINARY)
        .map_err(|e| e.to_string())
}

/// The newest tag the channel offers (`v0.7.0`).
pub async fn latest_tag() -> Result<String, String> {
    let (base, custom) = release_base();
    let client = client()?;
    if custom {
        let resp = client
            .get(&format!("{base}/latest"))
            .await
            .map_err(|e| e.to_string())?;
        let tag = String::from_utf8_lossy(&resp.body).trim().to_string();
        if tag.is_empty() {
            return Err(format!("{base}/latest is empty"));
        }
        return Ok(tag);
    }
    let resp = client
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .await
        .map_err(|e| format!("asking the release channel: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("release listing: {e}"))?;
    json.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "the release listing names no tag".into())
}

fn tag_of(version: &str) -> String {
    let v = version.trim();
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// Download one artifact and its signature.
pub async fn download(tag: &str, arch: &str) -> Result<(Vec<u8>, String), String> {
    let (base, _) = release_base();
    let client = client()?;
    let artifact = mesh_core::deploy::artifact_name(tag, arch);
    let url = format!("{base}/{tag}/{artifact}");
    let bin = client
        .get(&url)
        .await
        .map_err(|e| format!("downloading {artifact}: {e}"))?
        .body;
    let sig = client
        .get(&format!("{url}.minisig"))
        .await
        .map_err(|e| format!("downloading {artifact}.minisig: {e}"))?
        .body;
    let sig = String::from_utf8(sig).map_err(|_| "the signature is not text".to_string())?;
    Ok((bin, sig))
}

/// A verified release binary: downloaded, checked against a key the fleet
/// trusts. `(bytes, minisig, bare version)`.
///
/// `version` empty means this Director's own build first — the version
/// every other surface shows — and the channel's latest if that tag was
/// never published.
pub async fn fetch_verified(
    engine: &Engine,
    version: Option<&str>,
    arch: &str,
) -> Result<(Vec<u8>, String, String), String> {
    let keys = trusted_keys(engine).await;
    if keys.is_empty() {
        return Err(
            "nothing here could verify a download: this build carries the placeholder release \
             key and the mesh trusts none yet. Trust your signing key first (Updates → Release \
             keys), then try again."
                .into(),
        );
    }
    let tag = match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => tag_of(v),
        None => tag_of(env!("CARGO_PKG_VERSION")),
    };
    let ((bin, sig), tag) = match download(&tag, arch).await {
        Ok(f) => (f, tag),
        Err(first) if version.is_none() => {
            // Director's own version was never released; the channel's
            // newest is the honest fallback, said out loud.
            let latest = latest_tag()
                .await
                .map_err(|e| format!("{first}; and {e}"))?;
            if latest == tag {
                return Err(first);
            }
            tracing::info!("release {tag} is not on the channel; using {latest}");
            (download(&latest, arch).await?, latest)
        }
        Err(e) => return Err(e),
    };
    mesh_core::release::verify_minisign_any(&bin, &sig, &keys).map_err(|e| e.to_string())?;
    let bare = tag.trim_start_matches(['v', 'V']).to_string();
    Ok((bin, sig, bare))
}

// ---------------------------------------------------------------------------
// Publishing bytes into the mesh
// ---------------------------------------------------------------------------

/// Put one verified binary into the mesh as the release for `version` on
/// `arch`: stage it under its artifact name, push it as a content
/// bundle, append the admin-signed record (M24 §9).
///
/// The one path both the file picker and the channel fetch go through,
/// so a release published either way is the same record.
pub async fn publish_bytes(
    engine: &Engine,
    bytes: &[u8],
    minisig: &str,
    version: &str,
    arch: &str,
) -> Result<String, String> {
    let bare = version.trim().trim_start_matches(['v', 'V']).to_string();
    let arch = mesh_core::deploy::normalise_arch(arch).to_string();
    let keys = trusted_keys(engine).await;
    mesh_core::release::verify_minisign_any(bytes, minisig, &keys).map_err(|e| {
        format!("{e}\n\nAdd the signing key under Release keys, then publish again.")
    })?;

    // Already there for this architecture? Then nothing to do, and the
    // sentence says so rather than the merge silently keeping the first.
    if let Ok(IpcResponse::Releases(rows)) = engine.ipc(IpcRequest::Releases).await
        && rows
            .iter()
            .any(|r| r.version == bare && (r.arch.is_empty() || r.arch == arch))
    {
        return Err(format!(
            "{bare} is already published in this mesh for {arch} — a version names one binary \
             per architecture for the life of the mesh."
        ));
    }

    let artifact = mesh_core::deploy::artifact_name(&bare, &arch);
    let staging = std::env::temp_dir().join(format!(
        "meshware-release-{}-{}",
        std::process::id(),
        mesh_proto::now_micros()
    ));
    std::fs::create_dir_all(&staging).map_err(|e| format!("staging: {e}"))?;
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&staging);
    };
    if let Err(e) = std::fs::write(staging.join(&artifact), bytes) {
        cleanup();
        return Err(format!("staging the release binary: {e}"));
    }
    let abs = match std::fs::canonicalize(&staging) {
        Ok(p) => p,
        Err(e) => {
            cleanup();
            return Err(format!("resolving the staging directory: {e}"));
        }
    };
    let pushed = engine
        .ipc(IpcRequest::Push {
            path: abs.to_string_lossy().into_owned(),
        })
        .await;
    cleanup();
    let IpcResponse::Pushed {
        bundle_id,
        total_size,
        ..
    } = pushed?
    else {
        return Err("unexpected response to push".into());
    };

    let mut release = mesh_proto::ReleaseRecord {
        version: bare.clone(),
        bundle_id,
        artifact,
        minisig: minisig.to_string(),
        digest: *blake3::hash(bytes).as_bytes(),
        published_by: engine.user.user_id(),
        published_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    release.signature = engine.user.sign(&release.signing_bytes()).to_vec();
    engine
        .ipc(IpcRequest::ReleasePublish {
            release: Box::new(release),
        })
        .await?;
    Ok(format!(
        "published meshnode {bare} for {arch} into the mesh ({}, {} MB) — nodes fetch it from \
         each other",
        bundle_id.short(),
        total_size / 1_048_576
    ))
}

/// The architecture a binary file is for, from its name or from this
/// machine's when the name does not say.
pub fn arch_of_file(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    mesh_core::deploy::arch_of_artifact(&name)
        .unwrap_or(std::env::consts::ARCH)
        .to_string()
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    mesh_proto::version_cmp(a, b)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Where the fleet stands against what exists.
#[tauri::command]
pub async fn release_check(state: State<'_, Director>) -> Result<ReleaseCheckView, String> {
    let engine = state.engine().await?;
    let nodes = match engine.ipc(IpcRequest::Nodes).await? {
        IpcResponse::Nodes(rows) => rows,
        _ => Vec::new(),
    };
    let live: Vec<_> = nodes.iter().filter(|n| !n.left && !n.revoked).collect();
    let fleet_newest = live
        .iter()
        .filter_map(|n| n.version.clone())
        .max_by(|a, b| version_cmp(a, b));
    let mut fleet_arches: Vec<String> = live
        .iter()
        .filter_map(|n| n.capacity.as_ref())
        .map(|c| mesh_core::deploy::normalise_arch(&c.arch).to_string())
        .filter(|a| !a.is_empty())
        .collect();
    fleet_arches.sort();
    fleet_arches.dedup();
    if fleet_arches.is_empty() {
        fleet_arches.push("x86_64".into());
    }

    let mut published: Vec<PublishedReleaseView> = Vec::new();
    if let IpcResponse::Releases(rows) = engine.ipc(IpcRequest::Releases).await? {
        for r in rows {
            match published.iter_mut().find(|p| p.version == r.version) {
                Some(p) => {
                    if !r.arch.is_empty() && !p.arches.contains(&r.arch) {
                        p.arches.push(r.arch);
                    }
                }
                None => published.push(PublishedReleaseView {
                    version: r.version,
                    arches: if r.arch.is_empty() {
                        vec![]
                    } else {
                        vec![r.arch]
                    },
                }),
            }
        }
    }
    published.sort_by(|a, b| version_cmp(&b.version, &a.version));

    let keys = trusted_keys(&engine).await;
    let can_verify = !keys.is_empty();
    let verify_note = (!can_verify).then(|| {
        "This build carries the placeholder release key and the mesh trusts none yet, so a \
         download could not be verified. Trust your signing key under Release keys first."
            .to_string()
    });

    let (channel_latest, channel_error) = match latest_tag().await {
        Ok(tag) => (Some(tag.trim_start_matches(['v', 'V']).to_string()), None),
        Err(e) => (None, Some(e)),
    };

    Ok(ReleaseCheckView {
        director_version: env!("CARGO_PKG_VERSION").to_string(),
        fleet_newest,
        published,
        channel_latest,
        channel_error,
        can_verify,
        verify_note,
        fleet_arches,
    })
}

/// Fetch a release from the channel for each architecture and publish it
/// into the mesh, reporting as it goes.
#[tauri::command]
pub async fn release_fetch(
    app: tauri::AppHandle,
    state: State<'_, Director>,
    version: String,
    arches: Vec<String>,
    channel: Channel<ReleaseFetchEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let arches: Vec<String> = if arches.is_empty() {
        vec!["x86_64".into()]
    } else {
        arches
            .into_iter()
            .map(|a| mesh_core::deploy::normalise_arch(&a).to_string())
            .collect()
    };
    tokio::spawn(async move {
        let mut published = Vec::new();
        let mut notes = Vec::new();
        let mut version_seen = version.trim().trim_start_matches(['v', 'V']).to_string();
        for arch in arches {
            let say = |stage: &str, detail: String| {
                let _ = channel.send(ReleaseFetchEvent::Progress {
                    arch: arch.clone(),
                    stage: stage.into(),
                    detail,
                });
            };
            say("downloading", format!("meshnode {version_seen} for {arch}"));
            let (bytes, sig, bare) = match fetch_verified(
                &engine,
                Some(version.as_str()).filter(|v| !v.trim().is_empty()),
                &arch,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = channel.send(ReleaseFetchEvent::Failed { message: e });
                    return;
                }
            };
            version_seen = bare.clone();
            say(
                "verifying",
                format!("{} MB, signature verified", bytes.len() / 1_048_576),
            );
            say("publishing", "into the mesh".into());
            match publish_bytes(&engine, &bytes, &sig, &bare, &arch).await {
                Ok(note) => {
                    say("published", note.clone());
                    published.push(arch.clone());
                    notes.push(note);
                }
                Err(e) if e.contains("already published") => {
                    say("skipped", e);
                    published.push(arch.clone());
                }
                Err(e) => {
                    let _ = channel.send(ReleaseFetchEvent::Failed { message: e });
                    return;
                }
            }
        }
        crate::notify::attention(
            &app,
            "Release in the mesh",
            &format!(
                "meshnode {version_seen} is published for {}",
                published.join(", ")
            ),
        );
        let _ = channel.send(ReleaseFetchEvent::Done {
            version: version_seen,
            arches: published,
            note: notes.join(" "),
        });
    });
    Ok(())
}
