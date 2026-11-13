//! The desktop entry and icons for a Director that no package installed.
//!
//! On Linux the shell does not read an icon out of the running binary. It
//! matches the window's application id — `meshware-director`, the program
//! name, which is what GTK reports on Wayland and puts in `WM_CLASS` on
//! X11 — against a `meshware-director.desktop` file, and draws whatever
//! that file's `Icon=` names from the icon theme. A packaged Director gets
//! both from the .deb/.rpm. A Director run from a build directory gets
//! neither, so the dock, the alt-tab strip and the overview show the
//! generic "executable" glyph next to a window that plainly has an icon of
//! its own. This writes the same two things a package would, under the
//! user's own `~/.local/share`, and leaves the system copies alone when the
//! binary is one a package put in `/usr` or `/opt`.
//!
//! Idempotent and byte-compared: a launch that changes nothing writes
//! nothing, and a launch from a different build path rewrites `Exec=` so
//! the entry keeps pointing at the Director that actually ran.

use std::path::{Path, PathBuf};

const APP_ID: &str = "meshware-director";

/// The PNGs the bundle ships, at the sizes an icon theme wants them.
const PNGS: &[(&str, &[u8])] = &[
    ("32x32", include_bytes!("../icons/32x32.png")),
    ("128x128", include_bytes!("../icons/128x128.png")),
    ("256x256", include_bytes!("../icons/128x128@2x.png")),
    ("512x512", include_bytes!("../icons/512x512.png")),
];
const SVG: &[u8] = include_bytes!("../icons/icon.svg");

/// Make sure the desktop knows what this Director looks like.
pub fn ensure() {
    match install() {
        Ok(Some(path)) => tracing::info!("desktop entry written: {}", path.display()),
        Ok(None) => {}
        Err(e) => tracing::info!("could not write the desktop entry ({e})"),
    }
}

/// `Ok(Some(path))` when the entry was written or refreshed; `Ok(None)`
/// when nothing needed doing or a package owns the job.
fn install() -> std::io::Result<Option<PathBuf>> {
    let exe = std::env::current_exe()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    if ["/usr/", "/opt/", "/app/", "/snap/"]
        .iter()
        .any(|p| exe.starts_with(p))
    {
        return Ok(None);
    }

    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .ok_or_else(|| std::io::Error::other("no home directory"))?;

    let mut changed = false;
    let icons = data_home.join("icons/hicolor");
    for (size, bytes) in PNGS {
        let dir = icons.join(size).join("apps");
        changed |= write_if_changed(&dir.join(format!("{APP_ID}.png")), bytes)?;
    }
    changed |= write_if_changed(
        &icons.join("scalable/apps").join(format!("{APP_ID}.svg")),
        SVG,
    )?;

    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Meshware Director\n\
         GenericName=Compute mesh console\n\
         Comment=Point N machines at each other, get a governed compute mesh.\n\
         Exec=\"{exe}\" %u\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;\n\
         Keywords=mesh;compute;cluster;meshware;\n\
         StartupNotify=true\n\
         StartupWMClass={APP_ID}\n",
        exe = exe.display(),
    );
    let apps = data_home.join("applications");
    let path = apps.join(format!("{APP_ID}.desktop"));
    changed |= write_if_changed(&path, entry.as_bytes())?;

    if changed {
        // Best effort, and not required: GNOME and KDE watch these
        // directories. The cache just makes the first lookup faster.
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&apps)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return Ok(Some(path));
    }
    Ok(None)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|have| have == bytes) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_icons_are_pngs_and_the_mark_is_an_svg() {
        for (size, bytes) in PNGS {
            assert!(bytes.starts_with(b"\x89PNG"), "{size} is not a PNG");
        }
        assert!(std::str::from_utf8(SVG).is_ok_and(|s| s.contains("<svg")));
    }

    #[test]
    fn an_unchanged_file_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("a/b.txt");
        assert!(write_if_changed(&p, b"one").expect("write"));
        assert!(!write_if_changed(&p, b"one").expect("write"));
        assert!(write_if_changed(&p, b"two").expect("write"));
    }
}
