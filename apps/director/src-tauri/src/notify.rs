//! OS notifications, for the moments a person walked away from.
//!
//! A deploy across a rack and a rollout across a fleet both take minutes,
//! and Director hides to the tray. The one thing worth interrupting for is
//! *it finished* — so that is the only thing that notifies, and only when
//! the window is not already in front of the person. A notification that
//! duplicates what is on screen is noise; one that arrives while the
//! window is hidden is the reason the tray exists.

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// Show `title` / `body` as an OS notification, unless the main window is
/// visible and focused — in which case the screen already says it.
pub fn attention(app: &AppHandle, title: &str, body: &str) {
    let in_front = app
        .get_webview_window("main")
        .map(|w| w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false))
        .unwrap_or(false);
    if in_front {
        return;
    }
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        // No notification daemon is not an error worth a banner.
        tracing::debug!("notification not shown: {e}");
    }
}
