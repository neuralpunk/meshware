//! `director://` — one object, one address (L2 P5).
//!
//! `meshnode` prints `→ director://jobs/job-…` as the last line of any
//! command that produced one viewable object. This is the other end: the
//! scheme is registered with the desktop, a URL opens the Director already
//! running (or starts one), and the frontend lands on that object's screen.
//!
//! **Parsed here rather than in TypeScript** because a link that resolves
//! to the wrong screen is worse than no link — the person who pasted it has
//! already told a colleague it works — and this is the half that can have a
//! test. The frontend owns the routing table; this owns what a link *is*.
//!
//! The URL carries no credentials, no host and no port. It names a kind and
//! an id, and it is meaningless to a machine that is not in the mesh: the
//! reader's own Director resolves it against the mesh it is already a
//! member of, or shows nothing. A link is not an invitation.

use std::sync::Mutex;

use serde::Serialize;
use ts_rs::TS;

/// The scheme, in one place. Changing it changes what `meshnode` prints
/// (`format::deep_link`), and the two must not be able to disagree.
pub const SCHEME: &str = "director";

/// One resolved link, as the frontend receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeepLinkView {
    /// `jobs` | `fanouts` | `nodes` | `scripts` | `deployments` — the kinds
    /// `meshnode`'s `DeepLinkKind` emits, and the ones the frontend routes.
    pub kind: String,
    /// The object: a full id, or a name.
    pub id: String,
}

/// Parse `director://<kind>/<id>`.
///
/// Deliberately hand-parsed rather than run through a URL crate. A custom
/// scheme is not a *special* scheme, so a generic parser's decisions about
/// hosts, authorities and empty paths are decisions this format does not
/// want and would have to be undone one at a time. Two segments, and
/// nothing else is accepted.
pub fn parse(url: &str) -> Option<DeepLinkView> {
    let rest = url
        .strip_prefix(&format!("{SCHEME}://"))
        .or_else(|| url.strip_prefix(&format!("{SCHEME}:")))?;
    // A desktop handler hands the URL over with whatever it had on the end
    // — a trailing slash from a chat client's linkifier, most often.
    let rest = rest.trim_end_matches('/');
    let (kind, id) = rest.split_once('/')?;
    if kind.is_empty() || id.is_empty() {
        return None;
    }
    if !KINDS.contains(&kind) {
        return None;
    }
    // An id is an opaque token in this product's own alphabets, or a name.
    // Anything with a path separator, a query or a fragment in it is not
    // one, and refusing it here is what keeps a link from being a way to
    // ask the frontend for something it was not designed to be asked.
    if id
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
    {
        return None;
    }
    Some(DeepLinkView {
        kind: kind.to_string(),
        id: id.to_string(),
    })
}

/// The kinds a link may name. Exactly `meshnode`'s `DeepLinkKind`, and
/// exactly the frontend's routing table — an unknown kind is refused here
/// rather than delivered for something downstream to shrug at.
const KINDS: [&str; 5] = ["jobs", "fanouts", "nodes", "scripts", "deployments"];

/// The link waiting to be acted on, if any.
///
/// A slot rather than an event stream, because a person who opens three
/// links in four seconds means the third one: the earlier two are already
/// stale by the time anything renders. Last writer wins, and the frontend
/// takes it on its next tick.
#[derive(Default)]
pub struct Pending(Mutex<Option<DeepLinkView>>);

impl Pending {
    pub fn set(&self, url: &str) {
        if let Some(link) = parse(url) {
            if let Ok(mut slot) = self.0.lock() {
                *slot = Some(link);
            }
        } else {
            tracing::info!("ignoring an unroutable link: {url}");
        }
    }

    /// Take it, leaving nothing behind: a link acted on twice would send
    /// somebody back to a screen they had already navigated away from.
    pub fn take(&self) -> Option<DeepLinkView> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Feed every `director://` argument of a command line into `pending`.
///
/// This is the shape both entry points have: the OS launches the app with
/// the URL in `argv` on Linux and Windows, and a second launch hands its
/// `argv` to the first through the single-instance plugin.
pub fn absorb_args<I: IntoIterator<Item = String>>(pending: &Pending, args: I) {
    for arg in args {
        if arg.starts_with(SCHEME) && parse(&arg).is_some() {
            pending.set(&arg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_link_resolves_to_a_job() {
        assert_eq!(
            parse("director://jobs/job-agqgubvgeywnxzaszhsolmv4uu"),
            Some(DeepLinkView {
                kind: "jobs".into(),
                id: "job-agqgubvgeywnxzaszhsolmv4uu".into(),
            })
        );
    }

    /// Every kind `meshnode` can print, because a link the CLI emits and
    /// this refuses is the failure this whole phase is about.
    #[test]
    fn every_kind_the_cli_emits_is_routable() {
        for kind in KINDS {
            let url = format!("director://{kind}/thing-1");
            assert!(parse(&url).is_some(), "{url}");
        }
    }

    /// A chat client that linkifies a URL routinely adds a trailing slash.
    #[test]
    fn a_trailing_slash_is_not_a_different_link() {
        assert_eq!(
            parse("director://jobs/job-1/"),
            parse("director://jobs/job-1")
        );
    }

    #[test]
    fn nothing_else_is_a_link() {
        for url in [
            "https://example.com/jobs/job-1",
            "director://",
            "director://jobs",
            "director://jobs/",
            "director:///job-1",
            // A kind nothing routes.
            "director://secrets/api-token",
            // Anything that is trying to be a path, a query or a fragment.
            "director://jobs/../../etc/passwd",
            "director://jobs/job-1?then=delete",
            "director://jobs/job-1#x",
            "director://jobs/a b",
        ] {
            assert_eq!(parse(url), None, "{url}");
        }
    }

    #[test]
    fn the_slot_is_taken_once() {
        let pending = Pending::default();
        pending.set("director://nodes/mw1abc");
        assert_eq!(pending.take().map(|l| l.id), Some("mw1abc".to_string()));
        assert_eq!(pending.take(), None);
    }

    /// Last writer wins: somebody who opens three links means the third.
    #[test]
    fn the_newest_link_is_the_one_that_matters() {
        let pending = Pending::default();
        pending.set("director://jobs/job-1");
        pending.set("director://jobs/job-2");
        assert_eq!(pending.take().map(|l| l.id), Some("job-2".to_string()));
    }

    /// An unroutable argument must not clear a good one, and must not
    /// become one.
    #[test]
    fn a_command_line_yields_only_its_links() {
        let pending = Pending::default();
        absorb_args(
            &pending,
            [
                "/usr/bin/meshware-director".to_string(),
                "--hidden".to_string(),
                "director://scripts/deploy".to_string(),
            ],
        );
        assert_eq!(pending.take().map(|l| l.kind), Some("scripts".to_string()));
    }
}
