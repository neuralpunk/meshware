//! Director updating itself.
//!
//! The Updates tab says "this Director's own peer updates with Director's
//! installer", which was the one machine still doing it by hand. The
//! updater plugin closes that: it asks the release channel for
//! `latest.json`, verifies the installer's signature against the same
//! minisign key every meshnode release is signed with, downloads,
//! installs, and restarts.
//!
//! The key is the one compiled into this binary (`packaging/minisign.pub`,
//! the file `meshnode` and `install.sh` also carry), handed to the plugin
//! at startup in the form it expects — so a key rotation is one file, not
//! two. A build carrying the placeholder key cannot verify anything and
//! says so rather than checking.
//!
//! Producing the artifacts is the release pipeline's job: `tauri build`
//! with `bundle.createUpdaterArtifacts` on and `TAURI_SIGNING_PRIVATE_KEY`
//! set writes the signed installers and `latest.json` the endpoint serves.

use serde::Serialize;
use tauri::AppHandle;
use tauri::ipc::Channel;
use tauri_plugin_updater::UpdaterExt;
use ts_rs::TS;

/// The public key in the updater's format: the minisign `.pub` file,
/// base64 as a whole.
pub fn pubkey() -> String {
    data_encoding::BASE64.encode(crate::channel::EMBEDDED_PUBKEY_FILE.as_bytes())
}

/// Where Director stands against the channel.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DirectorUpdateView {
    pub current: String,
    /// A newer version on the channel, when there is one.
    pub available: Option<String>,
    pub notes: Option<String>,
    pub published: Option<String>,
    /// A download could be verified: the embedded key is real.
    pub can_verify: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DirectorUpdateEvent {
    Progress {
        #[ts(type = "number")]
        downloaded: u64,
        #[ts(type = "number | null")]
        total: Option<u64>,
    },
    Installed {
        version: String,
    },
    Failed {
        message: String,
    },
}

fn placeholder_note() -> String {
    "This build carries the placeholder release key, so it cannot verify a download. Release \
     builds carry the real one."
        .into()
}

#[tauri::command]
pub async fn director_update_check(app: AppHandle) -> Result<DirectorUpdateView, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    if crate::channel::embedded_pubkey().is_none() {
        return Ok(DirectorUpdateView {
            current,
            available: None,
            notes: None,
            published: None,
            can_verify: false,
            note: Some(placeholder_note()),
        });
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(DirectorUpdateView {
            current,
            available: Some(update.version.clone()),
            notes: update.body.clone(),
            published: update.date.map(|d| d.to_string()),
            can_verify: true,
            note: None,
        }),
        Ok(None) => Ok(DirectorUpdateView {
            current,
            available: None,
            notes: None,
            published: None,
            can_verify: true,
            note: None,
        }),
        Err(e) => Err(format!("asking the release channel: {e}")),
    }
}

/// Download, install, restart. Progress on the channel until the restart
/// takes the window away.
#[tauri::command]
pub async fn director_update_install(
    app: AppHandle,
    channel: Channel<DirectorUpdateEvent>,
) -> Result<(), String> {
    if crate::channel::embedded_pubkey().is_none() {
        return Err(placeholder_note());
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let update = match updater.check().await {
            Ok(Some(u)) => u,
            Ok(None) => {
                let _ = channel.send(DirectorUpdateEvent::Failed {
                    message: "already on the newest version".into(),
                });
                return;
            }
            Err(e) => {
                let _ = channel.send(DirectorUpdateEvent::Failed {
                    message: format!("asking the release channel: {e}"),
                });
                return;
            }
        };
        let version = update.version.clone();
        let progress = channel.clone();
        let mut downloaded: u64 = 0;
        let result = update
            .download_and_install(
                |chunk, total| {
                    downloaded += chunk as u64;
                    let _ = progress.send(DirectorUpdateEvent::Progress { downloaded, total });
                },
                || {},
            )
            .await;
        match result {
            Ok(()) => {
                let _ = channel.send(DirectorUpdateEvent::Installed {
                    version: version.clone(),
                });
                // A breath, so the last frame reaches the screen before
                // the process that would deliver it is gone.
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                app.restart();
            }
            Err(e) => {
                let _ = channel.send(DirectorUpdateEvent::Failed {
                    message: format!("installing {version}: {e}"),
                });
            }
        }
    });
    Ok(())
}
