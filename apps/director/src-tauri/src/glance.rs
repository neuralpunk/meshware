//! The glance: Meshware in the menu bar.
//!
//! Everything else in Director answers a question you went looking for.
//! This answers the one you did not: *is my mesh alright?* — a resident
//! icon whose menu carries the mesh's state in words, and a way back to
//! the window from wherever it went.
//!
//! ## The icon is the app's mark, and it does not move
//!
//! It used to be a constellation rasterised from live membership: one dot
//! per node, colour for liveness, radius for saturation, redrawn every few
//! seconds. It was a nice idea and it was the wrong one, for two reasons.
//!
//! The small one is taste: a tray icon's first job is to be *recognisable*,
//! and an icon that is a different picture every time you look at it is one
//! you cannot learn. Twenty-two pixels is not enough resolution to carry a
//! reading anybody trusts, and the same facts are already in the menu, in
//! words, where they can be read exactly.
//!
//! The large one is mechanical. On Linux the tray is StatusNotifierItem,
//! and every `set_icon` with raw pixels makes `tray-icon` write a fresh
//! temporary PNG and hand its path to libayatana-appindicator. A five
//! second redraw loop is therefore a temp file every five seconds for the
//! life of the session, plus whatever the host does about being told its
//! icon changed — which on several panels is a visible flicker.
//!
//! So: one static image, from `meshware-icons/tray/director/`, chosen for
//! the platform's convention. The *menu* is still live, because that costs
//! one already-made request and is where the information belongs.
//!
//! ## The tray may not be visible, and that changes the window's behaviour
//!
//! A tray-resident app hides on close. That is only correct where there is
//! something to hide *to*, and on Linux there very often is not: GNOME
//! Shell has never implemented StatusNotifierItem, so on a stock GNOME
//! desktop `TrayIconBuilder::build` succeeds, the item is registered on the
//! session bus, and nothing whatsoever displays it. Director used to read
//! that success as "there is a tray", hide the window into it on close, and
//! leave the user with a running process and no way back that they would
//! think of.
//!
//! [`sni_host_present`] asks the bus the question directly — is there a
//! StatusNotifierWatcher, and does it have a host — and the answer gates
//! close-to-hide and `--hidden` in `lib.rs`. It is re-checked on the same
//! tick as the menu, so installing the GNOME extension mid-session starts
//! working without a restart.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use mesh_proto::{IpcRequest, IpcResponse, Liveness};

use crate::engine::Director;

/// How often the menu text and the tray's visibility are re-read.
///
/// Five seconds, matched to gossip: liveness itself does not move faster
/// than a gossip round, so a one-second poll would produce the same words
/// four times out of five and spend a wake-up doing it.
const REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The icon
// ---------------------------------------------------------------------------

/// The monochrome white glyph the icon set ships for dark panels, at 44 px,
/// so a HiDPI panel has real pixels to scale from rather than a blurred 22.
///
/// White is right for every dark panel, which is GNOME's default. It is the
/// wrong choice on a light KDE panel; that is a second asset the set does
/// not ship yet rather than something to fix by guessing here.
const TRAY_PNG: &[u8] =
    include_bytes!("../../../../meshware-icons/tray/director/director-tray-white-44.png");

/// The tray image, decoded once.
fn icon() -> Image<'static> {
    // Decoded rather than handed over as PNG bytes because `Image::Rgba` is
    // what every backend wants in the end, and doing it here means a
    // corrupt asset is a startup failure with a message rather than an
    // invisible tray.
    Image::from_bytes(TRAY_PNG).expect("the bundled tray PNG decodes")
}

// ---------------------------------------------------------------------------
// Is anything going to show it?
// ---------------------------------------------------------------------------

/// Will a tray icon actually appear on this desktop?
///
/// Linux trays are StatusNotifierItem: an app registers an item on the
/// session bus and a *host* — the panel — draws it. `org.kde.StatusNotifierWatcher`
/// is the rendezvous, and `IsStatusNotifierHostRegistered` is its own
/// answer to "is anybody drawing these". KDE, Xfce and the GNOME
/// AppIndicator extension all provide one; stock GNOME Shell does not, and
/// on it every SNI item is registered and invisible.
pub fn sni_host_present() -> bool {
    // On a thread of its own, always.
    //
    // zbus's blocking API drives a future to completion, and *which*
    // executor it uses depends on features that are decided by the whole
    // dependency graph rather than by this crate: `keyring`'s secret-service
    // backend also pulls zbus, and if its tokio feature unifies in, the
    // blocking call would try to block on a tokio handle. Doing that from a
    // runtime thread is a panic. A plain `std::thread` has no runtime
    // context to confuse anything, and the join costs one local round trip.
    std::thread::spawn(probe).join().unwrap_or(false)
}

fn probe() -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return false;
    };
    let Ok(proxy) = zbus::blocking::Proxy::<'_>::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    ) else {
        return false;
    };
    // No watcher on the bus at all → `ServiceUnknown`, which is the stock
    // GNOME case and the commonest one. A watcher that says `false` is a
    // panel that has the extension loaded but no host yet.
    proxy
        .get_property::<bool>("IsStatusNotifierHostRegistered")
        .unwrap_or(false)
}

/// What to tell somebody whose tray is not being drawn.
///
/// Named per desktop where the remedy is specific, because "install an
/// extension" without saying which one is the same as saying nothing.
pub fn missing_host_hint() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.to_ascii_lowercase().contains("gnome") {
        "GNOME does not draw tray icons on its own. Install the \
         “AppIndicator and KStatusNotifierItem Support” extension \
         (Fedora: `sudo dnf install gnome-shell-extension-appindicator`, then enable it \
         in Extensions and log out and back in)."
            .to_string()
    } else {
        "This desktop has no StatusNotifierItem host running, so tray icons are not drawn. \
         KDE, Xfce and most panels provide one; GNOME needs the AppIndicator extension."
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What the menu is currently saying.
///
/// Compared, not just stored: an unchanged snapshot means every menu line
/// would come out identical, and the cheapest update is the one that does
/// not happen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// One entry per node, in a stable order: `(alive, saturation 0–100)`.
    nodes: Vec<(bool, u32)>,
    running: u32,
    queued: u32,
    /// Mesh-wide saturation: busy slots over available slots.
    saturation: u32,
    mesh_name: String,
    /// No engine yet (before the wizard finishes, or after a drain).
    stopped: bool,
}

impl Snapshot {
    fn alive(&self) -> usize {
        self.nodes.iter().filter(|(alive, _)| *alive).count()
    }

    /// The tooltip and the menu's first line: the whole glance in words,
    /// for a screen reader and for the hover.
    pub fn summary(&self) -> String {
        if self.stopped {
            return "Meshware — not running".to_string();
        }
        if self.nodes.is_empty() {
            return "Meshware — no mesh yet".to_string();
        }
        let mut line = format!(
            "{} · {}/{} node{} · {}% busy",
            self.mesh_name,
            self.alive(),
            self.nodes.len(),
            if self.nodes.len() == 1 { "" } else { "s" },
            self.saturation
        );
        match (self.running, self.queued) {
            (0, 0) => line.push_str(" · idle"),
            (r, 0) => line.push_str(&format!(" · {r} running")),
            (r, q) => line.push_str(&format!(" · {r} running, {q} queued")),
        }
        line
    }
}

/// Menu handles kept so their text can be rewritten in place.
struct Items<R: Runtime> {
    summary: MenuItem<R>,
    detail: MenuItem<R>,
}

/// Live tray state.
pub struct Glance<R: Runtime> {
    tray: TrayIcon<R>,
    items: Items<R>,
    last: Mutex<Option<Snapshot>>,
    /// Whether anything on this desktop is actually drawing the item. Read
    /// by the window's close handler, so it must be cheap and lock-free.
    visible: AtomicBool,
}

/// Is the tray icon being drawn? `false` also when there is no tray at all.
pub fn is_visible<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.try_state::<Glance<R>>()
        .map(|g| g.visible.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Build the tray and start its refresh loop.
///
/// Failure is not fatal: a desktop with no tray at all is a desktop where
/// Director still works, and so is one that registers the item and draws
/// nothing. Both end with the window keeping its ordinary close-quits
/// behaviour — see [`is_visible`] and the module docs.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "glance:show", "Open Director", true, None::<&str>)?;
    let summary = MenuItem::with_id(app, "glance:summary", "Meshware", false, None::<&str>)?;
    let detail = MenuItem::with_id(app, "glance:detail", "starting…", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "glance:quit", "Quit Meshware", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &summary,
            &detail,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let empty = Snapshot {
        stopped: true,
        ..Snapshot::default()
    };
    let tray = TrayIconBuilder::with_id("glance")
        .icon(icon())
        .tooltip(empty.summary())
        .menu(&menu)
        // Left-click opens the window and the menu is the right-click —
        // on the platforms that deliver clicks at all. Linux SNI has no
        // activate event through this path, which is why "Open Director"
        // is the menu's own first action rather than only a shortcut.
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button, .. } = event
                && button == tauri::tray::MouseButton::Left
            {
                reveal(tray.app_handle());
            }
        })
        .on_menu_event(
            |app: &AppHandle<R>, event: MenuEvent| match event.id().as_ref() {
                "glance:show" => reveal(app),
                "glance:quit" => app.exit(0),
                _ => {}
            },
        )
        .build(app)?;

    // Asked once here, synchronously, because `lib.rs` needs the answer
    // immediately: whether `--hidden` may hide the window at startup.
    let visible = sni_host_present();
    if !visible {
        tracing::warn!(
            "the tray icon is registered but nothing on this desktop draws it — {}",
            missing_host_hint()
        );
    }

    let glance = Glance {
        tray,
        items: Items { summary, detail },
        last: Mutex::new(None),
        visible: AtomicBool::new(visible),
    };
    app.manage(glance);

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH);
        loop {
            ticker.tick().await;
            let snapshot = read(&handle).await;
            // Re-checked every tick so that installing the GNOME extension
            // mid-session starts working without restarting Director. The
            // probe owns its own thread; this keeps the join off a worker.
            let visible = tauri::async_runtime::spawn_blocking(sni_host_present)
                .await
                .unwrap_or(false);
            if let Some(glance) = handle.try_state::<Glance<R>>() {
                glance.visible.store(visible, Ordering::Relaxed);
                glance.apply(snapshot);
            }
        }
    });
    Ok(())
}

impl<R: Runtime> Glance<R> {
    /// Push a snapshot to the menu, skipping the work when nothing moved.
    ///
    /// The icon is not touched: it is static, and on Linux every needless
    /// `set_icon` is a temp file (see the module docs).
    fn apply(&self, snapshot: Snapshot) {
        {
            let mut last = self.last.lock().expect("glance lock");
            if last.as_ref() == Some(&snapshot) {
                return;
            }
            *last = Some(snapshot.clone());
        }
        let summary = snapshot.summary();
        let _ = self.tray.set_tooltip(Some(&summary));
        let _ = self.items.summary.set_text(&summary);
        let _ = self.items.detail.set_text(detail_line(&snapshot));
    }
}

/// The menu's second line: what is actually happening, in words.
fn detail_line(s: &Snapshot) -> String {
    if s.stopped {
        return "the engine is stopped".to_string();
    }
    if s.nodes.is_empty() {
        return "finish the wizard to start".to_string();
    }
    let down = s.nodes.len() - s.alive();
    match (down, s.running, s.queued) {
        // A node not answering is the one thing worth saying first, always.
        (d, _, _) if d > 0 => format!("{d} node{} not answering", if d == 1 { "" } else { "s" }),
        (_, 0, 0) => "everything idle".to_string(),
        (_, r, 0) => format!("{r} job{} running", if r == 1 { "" } else { "s" }),
        (_, r, q) => format!("{r} running · {q} waiting"),
    }
}

/// Ask the embedded engine what the mesh looks like.
async fn read<R: Runtime>(app: &AppHandle<R>) -> Snapshot {
    let Some(state) = app.try_state::<Director>() else {
        return Snapshot {
            stopped: true,
            ..Snapshot::default()
        };
    };
    let Ok(engine) = state.engine().await else {
        return Snapshot {
            stopped: true,
            ..Snapshot::default()
        };
    };
    let Ok(IpcResponse::Top(rows)) = engine.ipc(IpcRequest::Top).await else {
        return Snapshot {
            stopped: true,
            ..Snapshot::default()
        };
    };
    let mesh_name = match engine.ipc(IpcRequest::Status).await {
        Ok(IpcResponse::Status(s)) => s.mesh_name.unwrap_or_default(),
        _ => String::new(),
    };

    let mut nodes = Vec::with_capacity(rows.len());
    let (mut running, mut queued, mut busy, mut slots) = (0u32, 0u32, 0u32, 0u32);
    for row in &rows {
        let alive = row.is_self || row.liveness == Liveness::Alive;
        // A non-executing peer has no saturation to report and must not
        // drag the mesh figure toward zero — it was never going to run
        // anything (D6). It still counts as a node, because it is one.
        let sat = if row.no_execute || row.slots == 0 {
            0
        } else {
            (row.running * 100 / row.slots).min(100)
        };
        nodes.push((alive, sat));
        running = running.saturating_add(row.running);
        queued = queued.saturating_add(row.queued);
        if alive && !row.no_execute {
            busy = busy.saturating_add(row.running);
            slots = slots.saturating_add(row.slots);
        }
    }
    Snapshot {
        nodes,
        running,
        queued,
        // A mesh with no available slots is not "0% busy" by arithmetic —
        // it is a mesh with nothing to be busy *with*, and the honest
        // rendering of that is an idle menu rather than a division by zero.
        saturation: (busy * 100).checked_div(slots).unwrap_or(0).min(100),
        mesh_name,
        stopped: false,
    }
}

/// Bring the main window back, wherever it went.
pub(crate) fn reveal<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(nodes: &[(bool, u32)], running: u32, queued: u32) -> Snapshot {
        let slots = nodes.len() as u32 * 4;
        Snapshot {
            nodes: nodes.to_vec(),
            running,
            queued,
            saturation: (running * 100).checked_div(slots).unwrap_or(0),
            mesh_name: "home".into(),
            stopped: false,
        }
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        let s = snap(&[(true, 20), (true, 0), (false, 0)], 2, 1);
        assert_eq!(
            s.summary(),
            "home · 2/3 nodes · 16% busy · 2 running, 1 queued"
        );
    }

    #[test]
    fn one_node_is_not_pluralised() {
        assert!(snap(&[(true, 0)], 0, 0).summary().contains("1/1 node ·"));
    }

    #[test]
    fn a_stopped_engine_says_so_rather_than_showing_an_empty_mesh() {
        let s = Snapshot {
            stopped: true,
            ..Snapshot::default()
        };
        assert_eq!(s.summary(), "Meshware — not running");
        assert_eq!(detail_line(&s), "the engine is stopped");
    }

    #[test]
    fn a_node_not_answering_is_the_first_thing_said() {
        // Ahead of running work, which is the whole point of the line.
        let s = snap(&[(true, 0), (false, 0), (true, 90)], 7, 0);
        assert_eq!(detail_line(&s), "1 node not answering");
    }

    #[test]
    fn an_identical_snapshot_compares_equal_so_the_update_is_skipped() {
        assert_eq!(snap(&[(true, 10)], 1, 0), snap(&[(true, 10)], 1, 0));
        assert_ne!(snap(&[(true, 10)], 1, 0), snap(&[(false, 10)], 1, 0));
    }

    /// The tray asset has to decode, be square, and have ink in it. A
    /// silently-empty bitmap is exactly the failure that looks like "no
    /// tray on this desktop" and is not.
    #[test]
    fn the_bundled_tray_icon_decodes_and_has_ink_in_it() {
        let img = icon();
        assert_eq!(img.width(), img.height(), "the tray glyph is square");
        assert!(
            img.width() >= 22,
            "too small for a HiDPI panel to scale from"
        );
        let opaque = img
            .rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0x20)
            .count();
        let total = (img.width() * img.height()) as usize;
        assert!(
            opaque > total / 20,
            "only {opaque} of {total} pixels have ink — the glyph is effectively blank"
        );
    }

    /// Whatever the desktop, asking must not panic or hang — it runs on
    /// every tick and inside the window's close path.
    #[test]
    fn asking_whether_a_host_exists_always_answers() {
        let _: bool = sni_host_present();
        assert!(!missing_host_hint().is_empty());
    }
}
