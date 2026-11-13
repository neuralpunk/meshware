//! One answer to "which build is newer".
//!
//! Four places used to hold their own dotted-number comparison — the
//! daemon's release rows, the CLI's fleet table, Director's binary
//! resolver and its release channel — and every one of them sorted
//! `0.7.0-rc.1` *above* `0.7.0`, because a pre-release tag parses as a
//! zero. A release candidate that outranks the release it precedes is how a
//! fleet ends up "current" on a build nobody meant to ship. Semantic
//! versioning has one rule for this and `semver` implements it; this module
//! is the one place the workspace asks.
//!
//! Lenient on input, because versions arrive from `--version` output, file
//! names, and hand-typed fields: a leading `v`, whitespace, and a missing
//! patch (`0.7`) are all read. Anything that still does not parse compares
//! as a plain string, below everything that does — a value the sort cannot
//! rank should never win.

use std::cmp::Ordering;

/// Parse a version as leniently as the inputs above need.
fn parse(v: &str) -> Option<semver::Version> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    if let Ok(p) = semver::Version::parse(v) {
        return Some(p);
    }
    // `0.7` → `0.7.0`, `1` → `1.0.0`: the shape a tag or a field may take.
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let dots = core.matches('.').count();
    if dots < 2 && !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.') {
        let padded = format!("{core}{}{rest}", ".0".repeat(2 - dots));
        return semver::Version::parse(&padded).ok();
    }
    None
}

/// A sortable key: parsed versions rank by semver, unparseable ones below
/// them by string.
pub fn version_key(v: &str) -> (Option<semver::Version>, String) {
    (parse(v), v.trim().to_string())
}

/// Compare two version strings: semver where both parse, a string
/// comparison otherwise, and anything unparseable below anything parsed.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        // Two spellings of one version (`0.7`, `v0.7.0`) are equal, not
        // ordered by their text: a sort that put them apart would show a
        // fleet as two groups running the same build.
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.trim().cmp(b.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_beat_strings() {
        assert_eq!(version_cmp("0.10.0", "0.9.0"), Ordering::Greater);
        assert_eq!(version_cmp("0.9.0", "0.10.0"), Ordering::Less);
        assert_eq!(version_cmp("0.6.0", "v0.6.0"), Ordering::Equal);
        assert_eq!(version_cmp(" 0.6.0 ", "0.6.0"), Ordering::Equal);
    }

    #[test]
    fn a_pre_release_sorts_below_its_release() {
        assert_eq!(version_cmp("0.7.0-rc.1", "0.7.0"), Ordering::Less);
        assert_eq!(version_cmp("0.7.0-rc.2", "0.7.0-rc.1"), Ordering::Greater);
        assert_eq!(version_cmp("0.7.0", "0.6.9"), Ordering::Greater);
    }

    /// The ordering `scripts/ship-node.sh` builds its whole scheme on.
    ///
    /// That script hands the fleet `0.7.3-dev.N` builds so it never has to
    /// commit a version bump, which only works because a pre-release sits
    /// strictly between the release before it and the release it names.
    /// Both halves matter: below and the fleet would refuse a dev build as
    /// a downgrade, above and cutting the real 0.7.3 would look like one.
    #[test]
    fn dev_builds_sit_between_the_releases_they_bridge() {
        assert_eq!(version_cmp("0.7.3-dev.1", "0.7.2"), Ordering::Greater);
        assert_eq!(version_cmp("0.7.3-dev.1", "0.7.3"), Ordering::Less);
        // Numeric pre-release identifiers compare numerically, not as text —
        // a string sort would put dev.10 below dev.9 and the fleet would
        // stop taking builds after the ninth.
        assert_eq!(
            version_cmp("0.7.3-dev.10", "0.7.3-dev.9"),
            Ordering::Greater
        );
    }

    #[test]
    fn short_versions_are_read_and_junk_ranks_last() {
        assert_eq!(version_cmp("0.7", "0.7.0"), Ordering::Equal);
        assert_eq!(version_cmp("1", "0.9.9"), Ordering::Greater);
        assert_eq!(version_cmp("unknown", "0.0.1"), Ordering::Less);
        assert_eq!(version_cmp("0.0.1", "unknown"), Ordering::Greater);
        assert_eq!(version_cmp("b", "a"), Ordering::Greater);
        let mut v = vec!["unknown", "0.10.0", "0.7.0-rc.1", "0.9.0", "0.7.0"];
        v.sort_by(|a, b| version_cmp(b, a));
        assert_eq!(v, ["0.10.0", "0.9.0", "0.7.0", "0.7.0-rc.1", "unknown"]);
    }
}
