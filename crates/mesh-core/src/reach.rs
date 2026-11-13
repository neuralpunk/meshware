//! Which machines can actually see the data (R4 P2).
//!
//! Work stealing means any worker may end up with any item, so a run over a
//! share that only some of your machines mount is a run where some items
//! fail for a reason that has nothing to do with the work. Four hundred
//! items, three of six nodes mounting the NAS, and the answer arrives as two
//! hundred identical "No such file or directory" lines an hour later.
//!
//! The fix is to ask first. What to ask about is this module's only
//! question, and the answer is deliberately coarse: **a mount is present or
//! absent.** It is not present for one file in a directory and absent for the
//! next, so asking about four hundred paths would be four hundred times the
//! work for the same answer. One common ancestor is the whole question.

use std::path::{Path, PathBuf};

/// The single directory worth asking candidates about, from the paths a run
/// will touch.
///
/// `None` when there is nothing useful to ask:
///
///   * no absolute paths at all — the items are hostnames, numbers, image
///     tags, anything that is not a file, which is most runs;
///   * paths with no common ancestor above the root. Two libraries on two
///     different mounts share only `/`, and every node can see `/`, so the
///     probe would pass everywhere and mean nothing. Asking a question whose
///     answer is always yes is worse than not asking: it costs a round trip
///     and buys false confidence.
///
/// In both cases the caller filters nobody, which is exactly today's
/// behaviour.
pub fn common_ancestor(paths: &[String]) -> Option<PathBuf> {
    let mut dirs = paths
        .iter()
        .map(|p| Path::new(p.trim()))
        .filter(|p| p.is_absolute())
        // The item is a file; the mount is its directory. Asking about the
        // file would answer "no" for a run that is about to *create* it.
        .map(|p| p.parent().unwrap_or(p).to_path_buf());

    let mut common = dirs.next()?;
    for dir in dirs {
        common = ancestor_of(&common, &dir);
        common.parent()?; // reached `/`
    }
    common.parent().is_some().then_some(common)
}

/// The deepest directory that is a prefix of both.
fn ancestor_of(a: &Path, b: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for (x, y) in a.components().zip(b.components()) {
        if x != y {
            break;
        }
        out.push(x);
    }
    out
}

/// What to actually ask a candidate about (R4 P2, corrected).
///
/// The first version of this asked only about [`common_ancestor`], and a
/// real partial mesh showed why that is not enough: **an unmounted mount
/// point is an empty directory that exists.** `/mnt/nas` is there whether
/// or not the NAS is, so a node that could see nothing at all answered
/// "yes" and was handed work it could not do. That is not an edge case; it
/// is what an absent mount looks like on every Linux machine.
///
/// So the probe asks about the directory *and* a few real items from the
/// list — first, middle, last. Still one round trip, and it distinguishes
/// "the mount is there" from "the data is there", which is the question
/// that was actually being asked.
///
/// Sampling rather than asking about all four hundred: a mount is present
/// or absent, and three points across the list catch the case where a
/// selector spans two of them. An item list too short to sample is asked
/// about in full.
pub fn probe_paths(items: &[String]) -> Vec<String> {
    let files: Vec<&String> = items
        .iter()
        .filter(|i| Path::new(i.trim()).is_absolute())
        .collect();
    if files.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(dir) = common_ancestor(items) {
        out.push(dir.to_string_lossy().into_owned());
    }
    for i in [0, files.len() / 2, files.len() - 1] {
        let p = files[i].trim().to_string();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_library_asks_about_the_library() {
        let got = common_ancestor(&v(&[
            "/srv/media/Travel Man/S04E04 Naples (Jack Dee).mkv",
            "/srv/media/Travel Man/S05E01 St Petersburg.mkv",
            "/srv/media/Other Show/pilot.mkv",
        ]));
        assert_eq!(got, Some(PathBuf::from("/srv/media")));
    }

    #[test]
    fn one_file_asks_about_its_directory() {
        // Not the file: a run that is about to create it would be told the
        // node cannot see something that does not exist yet.
        assert_eq!(
            common_ancestor(&v(&["/srv/media/a.mkv"])),
            Some(PathBuf::from("/srv/media"))
        );
    }

    #[test]
    fn items_that_are_not_paths_ask_nothing() {
        assert_eq!(common_ancestor(&v(&["web-01", "web-02", "web-03"])), None);
        assert_eq!(common_ancestor(&v(&["1", "2", "3"])), None);
        assert_eq!(common_ancestor(&v(&[])), None);
        assert_eq!(
            common_ancestor(&v(&["relative/path", "also/relative"])),
            None
        );
    }

    #[test]
    fn two_mounts_ask_nothing_rather_than_asking_about_the_root() {
        // `/` is visible everywhere, so this probe would pass on every node
        // and exclude nobody — a round trip that buys false confidence.
        assert_eq!(
            common_ancestor(&v(&["/srv/media/a.mkv", "/mnt/other/b.mkv"])),
            None
        );
    }

    #[test]
    fn the_probe_asks_about_real_files_not_just_the_directory() {
        // The bug a real partial mesh found: `/mnt/nas` exists as an empty
        // directory when the NAS is not mounted, so asking only about the
        // directory got a "yes" from a node that could see nothing.
        let items = v(&[
            "/mnt/nas/a.mkv",
            "/mnt/nas/b.mkv",
            "/mnt/nas/c.mkv",
            "/mnt/nas/d.mkv",
        ]);
        let asked = probe_paths(&items);
        assert_eq!(
            asked[0], "/mnt/nas",
            "the mount itself is still asked about"
        );
        assert!(asked.contains(&"/mnt/nas/a.mkv".to_string()));
        assert!(asked.contains(&"/mnt/nas/d.mkv".to_string()));
        assert!(asked.len() <= 4, "one round trip, not four hundred");
    }

    #[test]
    fn nothing_to_ask_about_when_no_item_is_a_path() {
        assert!(probe_paths(&v(&["web-01", "web-02"])).is_empty());
        assert!(probe_paths(&v(&[])).is_empty());
    }

    #[test]
    fn a_mixture_uses_only_the_paths() {
        assert_eq!(
            common_ancestor(&v(&["/srv/media/a.mkv", "not-a-path", "/srv/media/b.mkv"])),
            Some(PathBuf::from("/srv/media"))
        );
    }
}
