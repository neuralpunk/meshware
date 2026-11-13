//! Two runs side by side, and two nodes side by side — the arithmetic
//! behind `results diff` and `nodes diff`, kept pure so it can be tested
//! without a mesh.
//!
//! The question a diff answers is never "show me both"; it is **"what
//! changed"** (L3's thesis, applied twice). So the output is the rows that
//! differ, classified, with the rows that did not counted rather than
//! listed.

use std::collections::BTreeMap;

/// One side of a keyed outcome: how a target (an item, or a node) did in
/// one run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Side {
    pub ok: bool,
    pub exit_code: Option<i32>,
    /// What the target said — a digest of its stdout, or an object id —
    /// when the run kept one. `None` = not captured, and two `None`s
    /// compare as "no basis to say".
    pub fingerprint: Option<String>,
    pub node: String,
}

/// How a key fared between the two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Change {
    /// Succeeded both times with the same answer.
    Same,
    /// Succeeded both times with a different answer.
    Changed,
    /// Succeeded before, does not now.
    NowFailing,
    /// Failed before, succeeds now.
    NowPassing,
    /// Failed both times.
    StillFailing,
    /// In the first run only.
    OnlyBefore,
    /// In the second run only.
    OnlyAfter,
}

impl Change {
    pub fn label(self) -> &'static str {
        match self {
            Change::Same => "same",
            Change::Changed => "changed",
            Change::NowFailing => "now failing",
            Change::NowPassing => "now passing",
            Change::StillFailing => "still failing",
            Change::OnlyBefore => "only before",
            Change::OnlyAfter => "only after",
        }
    }

    /// Rows a person wants first: what got worse, then what changed, then
    /// what got better.
    fn rank(self) -> u8 {
        match self {
            Change::NowFailing => 0,
            Change::Changed => 1,
            Change::StillFailing => 2,
            Change::OnlyAfter => 3,
            Change::OnlyBefore => 4,
            Change::NowPassing => 5,
            Change::Same => 6,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Row {
    pub key: String,
    pub change: Change,
    pub before: Option<Side>,
    pub after: Option<Side>,
}

/// The tally, for the summary line.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Summary {
    pub same: usize,
    pub changed: usize,
    pub now_failing: usize,
    pub now_passing: usize,
    pub still_failing: usize,
    pub only_before: usize,
    pub only_after: usize,
    /// Keys that succeeded both times but carried no fingerprint on at
    /// least one side, so "same" is an assumption about the exit code only.
    pub uncompared: usize,
}

/// Compare two keyed runs. Rows come back worst-first, then by key.
pub fn compare(before: &[(String, Side)], after: &[(String, Side)]) -> (Vec<Row>, Summary) {
    let b: BTreeMap<&str, &Side> = before.iter().map(|(k, s)| (k.as_str(), s)).collect();
    let a: BTreeMap<&str, &Side> = after.iter().map(|(k, s)| (k.as_str(), s)).collect();
    let mut keys: Vec<&str> = b.keys().chain(a.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();

    let mut rows = Vec::with_capacity(keys.len());
    let mut summary = Summary::default();
    for key in keys {
        let (x, y) = (b.get(key).copied(), a.get(key).copied());
        let change = match (x, y) {
            (Some(x), Some(y)) => match (x.ok, y.ok) {
                (true, true) => match (&x.fingerprint, &y.fingerprint) {
                    (Some(p), Some(q)) if p != q => Change::Changed,
                    (Some(_), Some(_)) => Change::Same,
                    _ => {
                        summary.uncompared += 1;
                        Change::Same
                    }
                },
                (true, false) => Change::NowFailing,
                (false, true) => Change::NowPassing,
                (false, false) => Change::StillFailing,
            },
            (Some(_), None) => Change::OnlyBefore,
            (None, Some(_)) => Change::OnlyAfter,
            (None, None) => continue,
        };
        match change {
            Change::Same => summary.same += 1,
            Change::Changed => summary.changed += 1,
            Change::NowFailing => summary.now_failing += 1,
            Change::NowPassing => summary.now_passing += 1,
            Change::StillFailing => summary.still_failing += 1,
            Change::OnlyBefore => summary.only_before += 1,
            Change::OnlyAfter => summary.only_after += 1,
        }
        rows.push(Row {
            key: key.to_string(),
            change,
            before: x.cloned(),
            after: y.cloned(),
        });
    }
    rows.sort_by(|p, q| {
        p.change
            .rank()
            .cmp(&q.change.rank())
            .then_with(|| p.key.cmp(&q.key))
    });
    (rows, summary)
}

/// One differing fact between two nodes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FactRow {
    pub key: String,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// Compare two flattened fact documents (`collector.key` → value). Rows for
/// every key whose value differs or exists on one side only, sorted by
/// key; `same` counts the rest.
pub fn compare_facts(
    left: &[(String, String)],
    right: &[(String, String)],
) -> (Vec<FactRow>, usize) {
    let l: BTreeMap<&str, &str> = left.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let r: BTreeMap<&str, &str> = right
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut keys: Vec<&str> = l.keys().chain(r.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let mut rows = Vec::new();
    let mut same = 0;
    for key in keys {
        let (a, b) = (l.get(key).copied(), r.get(key).copied());
        if a == b {
            same += 1;
            continue;
        }
        rows.push(FactRow {
            key: key.to_string(),
            left: a.map(String::from),
            right: b.map(String::from),
        });
    }
    (rows, same)
}

/// Keys a person is usually looking for when a job failed on one box
/// only, ahead of the rest. Not a filter — an ordering.
pub fn is_likely_culprit(key: &str) -> bool {
    const HINTS: [&str; 10] = [
        "kernel", "driver", "glibc", "libc", "python", "cuda", "arch", "os_", "gpu", "version",
    ];
    let k = key.to_ascii_lowercase();
    HINTS.iter().any(|h| k.contains(h))
}

/// Hex, for a fingerprint a person might paste next to another.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(ok: bool, fp: Option<&str>) -> Side {
        Side {
            ok,
            exit_code: Some(if ok { 0 } else { 1 }),
            fingerprint: fp.map(String::from),
            node: "n".into(),
        }
    }

    #[test]
    fn classifies_every_transition() {
        let before = vec![
            ("a".to_string(), side(true, Some("x"))),
            ("b".to_string(), side(true, Some("x"))),
            ("c".to_string(), side(true, Some("x"))),
            ("d".to_string(), side(false, None)),
            ("e".to_string(), side(false, None)),
            ("gone".to_string(), side(true, Some("x"))),
        ];
        let after = vec![
            ("a".to_string(), side(true, Some("x"))),
            ("b".to_string(), side(true, Some("y"))),
            ("c".to_string(), side(false, None)),
            ("d".to_string(), side(true, Some("x"))),
            ("e".to_string(), side(false, None)),
            ("new".to_string(), side(true, Some("x"))),
        ];
        let (rows, s) = compare(&before, &after);
        assert_eq!(
            (
                s.same,
                s.changed,
                s.now_failing,
                s.now_passing,
                s.still_failing,
                s.only_before,
                s.only_after
            ),
            (1, 1, 1, 1, 1, 1, 1)
        );
        // Worst first.
        assert_eq!(rows[0].key, "c");
        assert_eq!(rows[0].change, Change::NowFailing);
        assert_eq!(rows[1].key, "b");
    }

    #[test]
    fn an_uncaptured_output_is_counted_not_guessed() {
        let before = vec![("a".to_string(), side(true, None))];
        let after = vec![("a".to_string(), side(true, Some("x")))];
        let (_, s) = compare(&before, &after);
        assert_eq!(s.same, 1);
        assert_eq!(s.uncompared, 1);
    }

    #[test]
    fn facts_diff_reports_only_differences() {
        let l = vec![
            ("system.kernel".to_string(), "6.12".to_string()),
            ("system.arch".to_string(), "x86_64".to_string()),
            ("system.only_left".to_string(), "1".to_string()),
        ];
        let r = vec![
            ("system.kernel".to_string(), "6.8".to_string()),
            ("system.arch".to_string(), "x86_64".to_string()),
        ];
        let (rows, same) = compare_facts(&l, &r);
        assert_eq!(same, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "system.kernel");
        assert_eq!(rows[1].right, None);
    }
}
