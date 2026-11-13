//! The desktop niceties: launch at login, the clipboard, where the log is.
//!
//! Each is one plugin call behind one command, so the frontend keeps its
//! single door (`invoke`) and needs no per-plugin JavaScript package or
//! capability entry. A tray-resident control app is judged on exactly
//! these details.

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::view::TrayStatusView;

/// Is Director set to launch at login?
#[tauri::command]
pub fn autostart_get(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Launch at login, or not. The entry starts Director with `--hidden`,
/// so it comes up in the tray rather than as a window over the desktop.
#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    }
    .map_err(|e| e.to_string())
}

/// Put text on the clipboard.
///
/// The webview's own `navigator.clipboard` works when the page is focused
/// and silently does not otherwise — a copy button that sometimes copies
/// is worse than none. The plugin talks to the OS clipboard directly.
#[tauri::command]
pub fn copy_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

/// Open a copy's address in the desktop's browser (Load balancing,
/// 2026-09-14).
///
/// "Does this copy answer?" is settled by looking at it, and the registry
/// already knows the address — so the screen offers the look rather than
/// asking somebody to retype `192.168.1.31:20417` into a browser. Only
/// `http`, `https` and `mailto` are handed on: the string crosses from the
/// webview, and `xdg-open` opens whatever it is given, `file:` URLs and
/// `.desktop` files included. `mailto` is the help menu's "Contact
/// support" (UX-7), and opens a mail client, not a file.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    if !url_is_openable(&url) {
        return Err(format!(
            "only http, https and mailto addresses can be opened, not {url:?}"
        ));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("that address has spaces or control characters in it".into());
    }
    let mut child = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open a browser (xdg-open): {e}"))?;
    // Reaped off the command thread: `xdg-open` returns once the browser has
    // the URL, and an unreaped child is a zombie for as long as Director runs.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The schemes [`open_url`] will hand to the desktop.
fn url_is_openable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Where Director writes its rolling log.
#[tauri::command]
pub fn director_log_dir() -> String {
    crate::log_dir().display().to_string()
}

/// Show the log folder in the desktop's file manager (DIR-11).
///
/// The one place a person is told to look when Director will not start,
/// and until now only its path could be copied. The folder is Director's
/// own; the window names nothing.
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let dir = crate::log_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut child = std::process::Command::new("xdg-open")
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open a file manager (xdg-open): {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Lines of frontend errors written per minute, at most (DIR-11).
const FRONTEND_LOG_PER_MINUTE: u32 = 60;
/// The longest message, and stack, one line may carry.
const FRONTEND_LOG_MAX_CHARS: usize = 4_000;

/// A frontend error, written to Director's own log (DIR-11).
///
/// A crash in the window used to stop at the webview's console, which
/// nobody opens and no bug report carries, so a report arrived with the
/// Rust half of the story and not the half that broke. `main.ts` forwards
/// every `error` and `unhandledrejection` here, and a view that throws
/// forwards its error from its boundary.
///
/// The page is not trusted to be polite about it: lines are capped per
/// minute and in length, and control characters are escaped, so a loop
/// that throws cannot fill the disk or forge lines in the log.
#[tauri::command]
pub fn frontend_log(kind: String, message: String, stack: Option<String>) {
    static WINDOW: std::sync::Mutex<(Option<std::time::Instant>, u32)> =
        std::sync::Mutex::new((None, 0));
    let mut window = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    if !admit_line(&mut window, std::time::Instant::now()) {
        return;
    }
    drop(window);
    let kind = log_safe(&kind, 40);
    let message = log_safe(&message, FRONTEND_LOG_MAX_CHARS);
    let stack = stack.map(|s| log_safe(&s, FRONTEND_LOG_MAX_CHARS));
    tracing::error!(target: "director::frontend", %kind, stack = stack.as_deref().unwrap_or(""), "{message}");
}

/// The per-minute budget behind [`frontend_log`]: a fresh minute starts a
/// fresh count, and the line that first goes over says so once.
fn admit_line(window: &mut (Option<std::time::Instant>, u32), now: std::time::Instant) -> bool {
    let fresh = window
        .0
        .is_none_or(|start| now.duration_since(start) >= std::time::Duration::from_secs(60));
    if fresh {
        *window = (Some(now), 0);
    }
    window.1 = window.1.saturating_add(1);
    if window.1 == FRONTEND_LOG_PER_MINUTE + 1 {
        tracing::warn!(
            target: "director::frontend",
            "more than {FRONTEND_LOG_PER_MINUTE} frontend errors this minute — the rest are dropped"
        );
    }
    window.1 <= FRONTEND_LOG_PER_MINUTE
}

/// `text`, cut to `max` characters with control characters escaped, so
/// one call is one line of the log however it was written.
fn log_safe(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars().take(max) {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Is the tray icon actually being drawn on this desktop?
///
/// Settings asks because "Start at login" launches Director with
/// `--hidden`, and hidden with no visible tray is a running process nobody
/// can get back to. Rather than quietly disabling the switch, the screen
/// says what is missing and how to fix it — the tray is a thing people
/// want, and the remedy is one package away on every desktop that lacks it.
#[tauri::command]
pub fn tray_status(app: AppHandle) -> TrayStatusView {
    let present = app.tray_by_id("glance").is_some();
    let visible = present && crate::glance::is_visible(&app);
    TrayStatusView {
        present,
        visible,
        hint: if visible {
            String::new()
        } else if present {
            crate::glance::missing_host_hint()
        } else {
            "This desktop has no system tray, so Director closes rather than hiding.".to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{FRONTEND_LOG_PER_MINUTE, admit_line, log_safe, url_is_openable};

    #[test]
    fn frontend_lines_are_budgeted_per_minute() {
        let t0 = std::time::Instant::now();
        let mut window = (None, 0);
        for _ in 0..FRONTEND_LOG_PER_MINUTE {
            assert!(admit_line(&mut window, t0));
        }
        assert!(!admit_line(&mut window, t0));
        assert!(!admit_line(
            &mut window,
            t0 + std::time::Duration::from_secs(59)
        ));
        assert!(admit_line(
            &mut window,
            t0 + std::time::Duration::from_secs(60)
        ));
    }

    #[test]
    fn a_frontend_line_is_one_bounded_line() {
        assert_eq!(
            log_safe("boom\nERROR forged line", 100),
            "boom\\nERROR forged line"
        );
        assert_eq!(log_safe("abcdef", 3), "abc…");
        assert_eq!(
            log_safe("could not start — ¿qué?", 100),
            "could not start — ¿qué?"
        );
        assert!(
            !log_safe("\u{1b}[31mred\r", 100)
                .chars()
                .any(char::is_control)
        );
    }

    #[test]
    fn only_web_and_mail_addresses_are_openable() {
        assert!(url_is_openable(
            "https://neuralpunk.github.io/meshware/guide.html"
        ));
        assert!(url_is_openable("HTTP://192.168.1.31:20417"));
        assert!(url_is_openable(
            "mailto:someone@example.com?subject=Director"
        ));
        assert!(!url_is_openable("file:///etc/passwd"));
        assert!(!url_is_openable("/usr/share/applications/evil.desktop"));
        assert!(!url_is_openable("javascript:alert(1)"));
    }
}
