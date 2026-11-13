//! Paths a **person** chose, addressed by a token (S3).
//!
//! Director's window is a web page, and the commands that took a
//! filesystem path as a string took it from that page. The picker was in
//! JavaScript and the backend accepted whatever came back, so "write this
//! file" and "read that file" were, in the end, arbitrary write and
//! arbitrary read as the Director user — one script injection away, and the
//! same window holds stored SSH and sudo passwords.
//!
//! `save_key_file` already showed the right shape: open the native dialog
//! **from Rust** and act only on what the dialog returned. This module is
//! that shape for the read half, where the path must survive across more
//! than one command — an inventory is picked once and then previewed,
//! re-previewed as the format toggles, and finally imported, and carrying a
//! 1.3 MB YAML file through the webview three times to avoid a path string
//! would be the wrong trade.
//!
//! So the path stays here and the webview holds a token: 128 random bits
//! that name *a choice the user made*, and resolve to nothing else. A token
//! for a path nobody picked does not exist, and one the ring has evicted
//! resolves to `None` — a stale handle, not a different file.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How many picks are addressable at once. A person picks a file, previews
/// it, maybe re-picks; a handful covers every real flow and the ring keeps
/// a long session from accumulating handles to files it will never reopen.
const MAX: usize = 64;

/// The registry. Lives on the managed Tauri state, beside `deep_links`, and
/// is a plain `std::sync::Mutex` for the same reason: it is touched from
/// dialog callbacks that are not async and must not wait on the engine.
#[derive(Debug, Default)]
pub struct Picked {
    ring: Mutex<VecDeque<(String, PathBuf)>>,
}

impl Picked {
    /// Remember a path the user picked; hand back the token that names it.
    pub fn remember(&self, path: &Path) -> String {
        let token = new_token();
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.push_back((token.clone(), path.to_path_buf()));
        while ring.len() > MAX {
            ring.pop_front();
        }
        token
    }

    /// Remember a pick and describe it for the window: the token, the
    /// file name, and the path as text to show — never to send back.
    pub fn remember_view(&self, path: &Path) -> crate::view::PickedView {
        crate::view::PickedView {
            token: self.remember(path),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            path: path.to_string_lossy().into_owned(),
        }
    }

    /// The path a token names, or the sentence the window shows when it
    /// names none (SEC-14).
    pub fn require(&self, token: &str) -> Result<PathBuf, String> {
        self.resolve(token.trim())
            .ok_or_else(|| "that file is no longer selected — pick it again".to_string())
    }

    /// The path a token names, if it still names one.
    pub fn resolve(&self, token: &str) -> Option<PathBuf> {
        // Empty is the "nothing picked" the frontend sends before a dialog
        // has run; answering `None` here keeps that out of every caller.
        if token.is_empty() {
            return None;
        }
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.iter()
            .find(|(t, _)| t == token)
            .map(|(_, p)| p.clone())
    }
}

/// 128 bits, lowercase hex. Unguessable rather than sequential: a token is
/// the only thing between a caller and a file, so it should not be possible
/// to arrive at one by counting.
fn new_token() -> String {
    let mut bytes = [0u8; 16];
    // A failure here would mean the OS has no entropy, which is not a state
    // this process can usefully continue from — but it is also not worth a
    // panic in a file picker. A zero token is still only ever *this* ring's
    // key, and the pick that produced it is the one the user just made.
    if getrandom::fill(&mut bytes).is_err() {
        tracing::warn!("no OS entropy for a picker token");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_the_path_it_was_made_for() {
        let picked = Picked::default();
        let a = picked.remember(Path::new("/tmp/one.yaml"));
        let b = picked.remember(Path::new("/tmp/two.yaml"));
        assert_ne!(a, b, "two picks must not share a token");
        assert_eq!(
            picked.resolve(&a).as_deref(),
            Some(Path::new("/tmp/one.yaml"))
        );
        assert_eq!(
            picked.resolve(&b).as_deref(),
            Some(Path::new("/tmp/two.yaml"))
        );
    }

    #[test]
    fn nothing_else_resolves() {
        let picked = Picked::default();
        picked.remember(Path::new("/etc/passwd"));
        assert!(picked.resolve("").is_none());
        assert!(picked.resolve("/etc/passwd").is_none());
        assert!(picked.resolve("00000000000000000000000000000000").is_none());
    }

    #[test]
    fn a_view_shows_the_path_and_sends_only_the_token() {
        let picked = Picked::default();
        let v = picked.remember_view(Path::new("/home/you/dist/meshnode"));
        assert_eq!(v.name, "meshnode");
        assert_eq!(v.path, "/home/you/dist/meshnode");
        assert_eq!(
            picked.require(&v.token).unwrap(),
            PathBuf::from("/home/you/dist/meshnode")
        );
        // A path is not a token, however real the file.
        let err = picked.require("/home/you/.ssh/id_ed25519").unwrap_err();
        assert!(err.contains("pick it again"), "{err}");
    }

    #[test]
    fn the_ring_is_bounded() {
        let picked = Picked::default();
        let first = picked.remember(Path::new("/tmp/0"));
        for i in 1..=MAX {
            picked.remember(Path::new(&format!("/tmp/{i}")));
        }
        assert!(
            picked.resolve(&first).is_none(),
            "the oldest pick should have aged out"
        );
    }
}
