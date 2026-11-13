//! What this build is, the terms it runs under, and where to get help (UX-7).
//!
//! Every answer here comes from the build. The version and the licence
//! identifier are the workspace manifest's; the licence texts are the
//! repository's `LICENSE-MIT` and `LICENSE-APACHE`, compiled in, so what a
//! person reads in the app is exactly what the build was released under.
//!
//! Meshware is open source, MIT OR Apache-2.0, with no plans, tiers or
//! machine limits — so there is nothing here to entitle and nothing to count.

use serde::Serialize;
use ts_rs::TS;

/// The MIT licence this build was released under, verbatim.
const LICENSE_MIT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../LICENSE-MIT"));
/// The Apache licence this build was released under, verbatim.
const LICENSE_APACHE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../LICENSE-APACHE"
));

/// Where people are sent for help. A fork or a downstream build can point
/// these elsewhere at build time without touching the source.
const DOCS_URL: &str = match option_env!("MESHWARE_DOCS_URL") {
    Some(v) => v,
    None => "https://neuralpunk.github.io/meshware/guide.html",
};
const ISSUES_URL: &str = match option_env!("MESHWARE_ISSUES_URL") {
    Some(v) => v,
    None => "https://github.com/neuralpunk/meshware/issues",
};
const SOURCE_URL: &str = match option_env!("MESHWARE_SOURCE_URL") {
    Some(v) => v,
    None => "https://github.com/neuralpunk/meshware",
};

/// Everything the About pane, the help menu and a diagnostics copy show.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AboutView {
    /// Director's version, from the workspace manifest.
    pub version: String,
    /// The version of the `meshnode` this Director carries — what it keeps
    /// this machine's node at, and so part of what "behind" is measured
    /// against. `None` for a build with none bundled.
    pub bundled_meshnode: Option<String>,
    /// The commit the build was made from, when the build stamped one.
    pub commit: Option<String>,
    /// Always `linux`; shown beside `arch` in the diagnostics copy.
    pub os: String,
    pub arch: String,
    /// The manifest's licence expression, `MIT OR Apache-2.0`.
    pub license_id: String,
    /// The licence in words, for a person rather than a tool.
    pub license_name: String,
    /// Both licence texts, MIT first.
    pub license_text: String,
    pub docs_url: String,
    pub issues_url: String,
    pub source_url: String,
}

fn about(bundled_meshnode: Option<String>) -> AboutView {
    AboutView {
        version: env!("CARGO_PKG_VERSION").into(),
        bundled_meshnode,
        commit: option_env!("MESHWARE_BUILD_COMMIT").map(str::to_string),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        license_id: env!("CARGO_PKG_LICENSE").into(),
        license_name: "Open source — MIT or Apache 2.0, at your option".into(),
        license_text: format!(
            "{}\n\n{}\n\n{}",
            LICENSE_MIT.trim_end(),
            "─".repeat(72),
            LICENSE_APACHE.trim_end()
        ),
        docs_url: DOCS_URL.into(),
        issues_url: ISSUES_URL.into(),
        source_url: SOURCE_URL.into(),
    }
}

/// What this build is (UX-7). Cheap and engine-free, so it answers before
/// the mesh does — the About pane is where somebody goes when it will not.
#[tauri::command]
pub fn about_view(app: tauri::AppHandle) -> AboutView {
    use tauri::Manager as _;
    let carried = crate::localnode::carried(app.path().resource_dir().ok().as_deref());
    about(carried.map(|c| c.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_licence_is_the_repository_licence() {
        let a = about(None);
        assert_eq!(a.license_id, "MIT OR Apache-2.0");
        assert!(
            a.license_text.starts_with("MIT License"),
            "{}",
            &a.license_text[..40]
        );
        assert!(a.license_text.contains("Apache License"));
        assert!(a.license_text.contains("Version 2.0, January 2004"));
        assert!(!a.version.is_empty());
    }

    #[test]
    fn help_points_somewhere_openable() {
        let a = about(None);
        for u in [&a.docs_url, &a.issues_url, &a.source_url] {
            assert!(u.starts_with("https://"), "{u}");
        }
    }
}
