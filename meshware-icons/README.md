# Meshware icon set — Node & Director

Direction **7b “Blade & Tower”**: an isometric 1U blade with one amber LED.
**Node** (the agent) is a single blade. **Director** (the GUI) is three blades stacked.

Teal `#12857a` · amber LED `#f0b254` · faces `#ffffff / #a5d0cc / #6cb3ad` on teal.
Geometry lives on a 100×100 grid; every file here is generated from `_gen.js`.
Renders at **≤ 32 px use a “small” cut** (bigger mark, thicker seams, larger LED) — always ship the
supplied PNG for a size instead of downscaling the 1024.

## app/  — application icons
`<app>/<app>-rounded.svg`            22.5 % radius tile (macOS / Windows 11 / most Linux themes)
`<app>/<app>-square.svg`             hard corners (Linux .desktop themes that add their own mask, docs)
`<app>/<app>-*-small.svg`            the ≤ 32 px cut of each
`<app>/png/rounded|square/`          16 24 32 48 64 128 256 512 1024
`<app>/<app>.ico`                    Windows — 16 32 48 256 (PNG-compressed entries)
`<app>/<app>.icns`                   macOS — icp4…ic10 + retina ic11–ic14

## tauri/ — drop-in `src-tauri/icons/` folders
`32x32.png 128x128.png 128x128-2x.png icon.png icon.ico icon.icns`
Rename `128x128-2x.png` → `128x128@2x.png` after copying (the `@` was not allowed here).
For Windows Store logos run `cargo tauri icon icons/icon.png` — it regenerates the whole folder from the 1024.

## web/  — favicons, per app
`favicon.svg favicon.ico favicon-16/32.png apple-touch-icon.png (180) icon-192.png icon-512.png site.webmanifest`
`head-snippet.html` has the <link> tags. Touch/manifest icons are square on purpose — iOS and Android apply their own mask.

## ui/   — marks for inside the application and on web pages
`<app>-mark.svg`        single-colour, uses `currentColor` → drop inline or as <img>, colour it with CSS
`<app>-mark-small.svg`  same, ≤ 24 px cut
`<app>-mark-teal.svg`   full-colour mark for light surfaces
`<app>-mark-white.svg`  full-colour mark for dark surfaces
`png/teal|white/`       16 20 24 32 48 64 128 256, transparent

## tray/ — status-bar / tray glyphs, monochrome, transparent
`<app>-tray-white-*.png`         16 22 24 32 44 48 — Windows tray, GNOME/KDE dark panels
`<app>TrayTemplate*.png`        black — macOS template images (16 + 32 @2x); set `setTemplate(true)` / `is_template`

## Rust notes
- **Tauri**: copy `tauri/<app>/icons` over `src-tauri/icons`; `tauri.conf.json → bundle.icon` lists them already by default.
- **egui / winit / iced**: load `ui/<app>/png/teal/<app>-teal-256.png` (or the rounded 256) with the `image` crate and pass RGBA to `window.set_window_icon` / `Icon::from_rgba`.
- **cargo-bundle**: `[package.metadata.bundle] icon = ["app/<app>/png/rounded/<app>-32.png", "...-128.png", "...-256.png", "...-512.png"]`.
- **Linux .desktop**: install `app/<app>/png/square/<app>-{16..512}.png` to `hicolor/<size>x<size>/apps/meshware-<app>.png` and `<app>-square.svg` to `hicolor/scalable/apps/`.
- **Windows exe resource**: `winres` / `embed-resource` → point at `app/<app>/<app>.ico`.
