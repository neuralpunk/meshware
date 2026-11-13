//! The output convention, at the job layer (M19 §5.2).
//!
//! **This used to live in `wfrun.rs`, and that was the bug.** The marker,
//! the implicit four, the 64 KiB object spillover and the object-reference
//! encoding were all built for M10's workflow run loop and reachable only
//! from a workflow *step*. Every other way of running something — an ad-hoc
//! `run`, a fan-out across six hundred nodes, a `parallel` item — produced
//! log text and nothing else, so "I ran that on 600 hosts and got 600 logs;
//! where is the answer?" had no answer at all.
//!
//! M19 moves the convention down here, where every job can reach it, and
//! `wfrun` *consumes* it instead of owning it. Nothing about the contract
//! changed: a step's outputs and an ad-hoc run's outputs are now the same
//! values produced by the same code, which is the only way they can be
//! guaranteed never to drift apart.
//!
//! ## Opt-in capture
//!
//! Reading a job's outputs means streaming its whole log back from the
//! executor. The run loop has always asked [`crate::wfrun::outputs_wanted`]
//! first and skipped the read when nothing downstream referenced the step;
//! the job-layer equivalent is [`Wanted`], carried on the job environment
//! as [`mesh_proto::OUT_WANT`]. Absent it, nothing is captured and nothing
//! is paid for — which is what keeps `meshnode run 'uname -a'` exactly as
//! fast as it was.

use std::collections::{BTreeMap, HashSet};

/// Declared outputs at or under this size are recorded verbatim. Above it
/// the value becomes an M9 ephemeral object and only the reference is
/// recorded — the op-log replicates to every node in the mesh, and a build
/// log does not belong there (M10 §4.2).
///
/// It is also [`crate::facts::MAX_FACT_DOC`], because that is the same
/// judgement about the same log (M18 §4.4).
///
/// ## Why it did not move in M31 (A2 item 5)
///
/// M26 wanted this lowered to a few KB, on the theory that the op-log was
/// carrying payload. It is not: anything over this threshold has always
/// spilled to an M9 object and only the reference is recorded, so the
/// premise the change rested on was already false.
///
/// And lowering it would silently change what a **fact** may carry, because
/// the two are one constant. If it is ever lowered, the constants separate
/// first and the fact side gets its own decision — a collector whose
/// document stops fitting is a different problem, with different people
/// affected, and it must not be decided as a side effect of tuning job
/// output.
pub const INLINE_OUTPUT_MAX: usize = 64 * 1024;

/// What a job writes to stdout to publish its declared outputs.
///
/// An explicit sentinel rather than "the last line that looks like JSON":
/// a job's stdout is also its log, and guessing which part of a log was
/// meant as data is the kind of cleverness that fails on the day the log
/// changes. Both forms are accepted after the marker, because both are
/// natural to emit:
///
/// ```text
/// echo '::meshware-output:: sha=abc123'
/// echo '::meshware-output:: {"sha": "abc123", "count": 7}'
/// ```
pub const OUTPUT_MARKER: &str = "::meshware-output::";

/// Prefix marking a recorded output value that is an object reference
/// rather than the value itself.
pub const OBJECT_REF_PREFIX: &str = "object:";

// ---------------------------------------------------------------------------
// Implicit outputs (M10 §4.6, now every job's)
// ---------------------------------------------------------------------------

/// The job's standard output, trimmed of trailing whitespace.
pub const OUT_STDOUT: &str = "stdout";
/// Standard output re-encoded as compact JSON — **present only when the
/// whole of stdout parses**.
///
/// Published alongside `stdout` because a script that prints structured
/// data almost always prints JSON, and without this every such job needs
/// a `json.*` task glued to its output purely to re-read what it just
/// wrote. Its *absence* is informative too — it means the script
/// printed something that is not JSON, which is the first thing to check
/// when a downstream field comes out empty.
pub const OUT_STDOUT_JSON: &str = "stdout_json";
/// Standard error, trimmed. What a `fail`-lane branch usually wants to read.
pub const OUT_STDERR: &str = "stderr";
/// The process exit status, as a decimal string.
pub const OUT_EXIT_CODE: &str = "exit_code";

/// Outputs a command or service job publishes without declaring them.
pub const IMPLICIT_OUTPUTS: &[&str] = &[OUT_STDOUT, OUT_STDOUT_JSON, OUT_STDERR, OUT_EXIT_CODE];

// ---------------------------------------------------------------------------
// What a fan-out step publishes (M28 §8)
// ---------------------------------------------------------------------------
//
// **A fan-out step used to publish nothing at all**, and that was the bug.
// The four implicit outputs are singular by construction — one stdout, one
// exit code — and a step that ran on forty nodes has forty of each, so the
// settle path recorded a tally and moved on. Meanwhile the *validator*
// believed the same four were available, because "what a step publishes"
// was answered without asking whether the step fanned out. So
// `{{steps.survey.outputs.stdout}}` passed publish, passed review, and
// failed at run time with "has no value at this point in the run".
//
// Both halves are wrong and both are fixed: the aggregate below is what a
// fan-out step actually publishes, and it is what the validator now
// checks references against. The shape is chosen for the thing people do
// next — a script that reads every node's answer — which is why `results`
// is JSON with the node names in it rather than a heap of concatenated
// text with no way to tell which machine said what.

/// Every target's standard output, in target order, newline separated.
/// The plain reading of "what did the fleet say".
pub const OUT_FANOUT_STDOUT: &str = OUT_STDOUT;
/// One JSON array of `{node, exit_code, stdout, …declared}`, which is the
/// form a downstream `json.query` or Python step can actually work with.
pub const OUT_RESULTS: &str = "results";
/// How the group ended, as decimal strings: a branch on "did any fail"
/// should not have to parse a sentence.
pub const OUT_SUCCEEDED: &str = "succeeded";
pub const OUT_FAILED: &str = "failed";
pub const OUT_SKIPPED: &str = "skipped";

/// What a step with `fanout:` publishes without declaring anything.
pub const FANOUT_OUTPUTS: &[&str] = &[
    OUT_FANOUT_STDOUT,
    OUT_RESULTS,
    OUT_SUCCEEDED,
    OUT_FAILED,
    OUT_SKIPPED,
];

/// Is `key` one of the four that arrive without being declared?
pub fn is_implicit(key: &str) -> bool {
    IMPLICIT_OUTPUTS.contains(&key)
}

/// Which wanted keys can only come out of the log.
///
/// `exit_code` is deliberately not one of them — it is already in the job
/// summary, and treating it as one would stream a log back across the mesh
/// for a value we already have.
pub fn needs_log(key: &str) -> bool {
    key == OUT_STDOUT || key == OUT_STDERR || key == OUT_STDOUT_JSON
}

// ---------------------------------------------------------------------------
// What a caller asked for (M19 §5.2)
// ---------------------------------------------------------------------------

/// The output keys a submission wants back, as it travels to the origin
/// daemon on the job environment.
///
/// **Why this rides the environment.** `--json`, `--outputs` and `--expect`
/// have to reach the daemon, and the obvious carrier is closed: the
/// `IpcRequest` an executor re-decodes out of `JobSpec::auth` to re-verify
/// authorization is positional, so growing `Run` would mean a 1.8 executor
/// could not decode a 1.9 origin's job. `JobSpec::env` is an existing
/// `Vec<(String, String)>`, so a reserved key inside it changes no encoding
/// at all — the same idiom M9 used for requirements and M12 for `--hosts`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted(Vec<String>);

impl Wanted {
    /// Nothing asked for: capture nothing, read no log, cost nothing.
    pub fn none() -> Wanted {
        Wanted(Vec::new())
    }

    /// The four implicit outputs — what `--json` asks for.
    pub fn implicit() -> Wanted {
        Wanted(IMPLICIT_OUTPUTS.iter().map(|k| (*k).to_string()).collect())
    }

    /// A caller's explicit list, plus the implicit four so a row always
    /// carries the obvious values alongside whatever was named.
    pub fn keys<I, S>(keys: I) -> Wanted
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Wanted::implicit();
        for key in keys {
            let key = key.as_ref().trim();
            if !key.is_empty() && !out.0.iter().any(|k| k == key) {
                out.0.push(key.to_string());
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|k| k == key)
    }

    /// Keys the caller named that are not one of the implicit four — the
    /// ones a `::meshware-output::` marker has to publish.
    pub fn declared(&self) -> Vec<String> {
        self.0.iter().filter(|k| !is_implicit(k)).cloned().collect()
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Render onto a job environment. Nothing is written when nothing was
    /// asked for, so an ordinary run's environment is byte-identical to
    /// what it was before M19.
    pub fn to_env(&self) -> Vec<(String, String)> {
        if self.is_empty() {
            return Vec::new();
        }
        vec![(mesh_proto::OUT_WANT.to_string(), self.0.join(","))]
    }

    /// [`Self::to_env`] plus the `--expect` expression, which travels the
    /// same way and for the same reason.
    pub fn to_env_with(&self, expect: Option<&str>) -> Vec<(String, String)> {
        let mut env = self.to_env();
        if let Some(expr) = expect.map(str::trim).filter(|e| !e.is_empty()) {
            env.push((mesh_proto::OUT_EXPECT.to_string(), expr.to_string()));
        }
        env
    }

    /// Take the reserved capture keys **off** a job environment, on the
    /// origin, before anything is judged, placed or dispatched.
    ///
    /// Capture and assertion are the origin's business — the log already
    /// flows through this node on its way to the client, and the assertion
    /// is a question about the whole set of rows, which no executor has.
    /// So the keys are consumed here and the spec that reaches every
    /// executor is byte-identical to what it was before M19.
    pub fn take_from_env(env: &mut Vec<(String, String)>) -> (Wanted, Option<String>) {
        let wanted = Wanted::from_env(env);
        let expect = env
            .iter()
            .find(|(k, _)| k == mesh_proto::OUT_EXPECT)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty());
        env.retain(|(k, _)| !k.starts_with(mesh_proto::OUT_ENV_PREFIX));
        (wanted, expect)
    }

    /// Recover from a job environment. An unset key means "capture
    /// nothing", which is every pre-M19 job.
    pub fn from_env(env: &[(String, String)]) -> Wanted {
        let Some((_, value)) = env.iter().find(|(k, _)| k == mesh_proto::OUT_WANT) else {
            return Wanted::none();
        };
        let mut out = Wanted::none();
        for key in value.split(',') {
            let key = key.trim();
            if !key.is_empty() && !out.0.iter().any(|k| k == key) {
                out.0.push(key.to_string());
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Marker parsing
// ---------------------------------------------------------------------------

/// Pull declared outputs out of a job's stdout.
///
/// Only keys the caller *declared* are kept: a job that prints
/// `::meshware-output:: secret=hunter2` without declaring `secret` has said
/// nothing that will be remembered, so an accidental print cannot become
/// replicated state. Later markers win, so a retry loop inside a script
/// behaves the way a reader expects.
pub fn parse_outputs(stdout: &str, declared: &[String]) -> Vec<(String, String)> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line
            .find(OUTPUT_MARKER)
            .map(|i| &line[i + OUTPUT_MARKER.len()..])
        else {
            continue;
        };
        let rest = rest.trim();
        if rest.starts_with('{') {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(rest)
            {
                for (k, v) in map {
                    found.insert(k, json_scalar(&v));
                }
            }
            continue;
        }
        if let Some((k, v)) = rest.split_once('=') {
            found.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    declared
        .iter()
        .filter_map(|key| found.remove(key).map(|value| (key.clone(), value)))
        .collect()
}

/// JSON values become the string a shell would have written: a string is
/// its contents, everything else is its JSON form. Quoting a string here
/// would make `sha=abc` and `{"sha":"abc"}` mean different things, which
/// would be a trap.
///
/// Shared with the workflow templater, so `{{steps.x.outputs.a.b}}` and a
/// `::meshware-output::` JSON marker render the same value the same way.
pub fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Trim and cap a captured stream so it can live in a record.
///
/// A run record replicates to every node, so an unbounded `stdout` output
/// would put a build log in the op-log. Anything longer than this is cut
/// with a marker that says so — a truncated value that admits it is far
/// better than a silently short one.
pub fn implicit_stream_value(raw: &str) -> String {
    const MAX: usize = 4096;
    let trimmed = raw.trim_end();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… truncated at {MAX} bytes — declare an output for the value you need",
        &trimmed[..cut]
    )
}

/// The implicit outputs derivable from a job's two streams and its exit
/// code, filtered to what was actually asked for.
///
/// One definition, called by the workflow run loop and by the ad-hoc job
/// path alike, so `{{steps.x.outputs.stdout_json}}` and
/// `meshnode run --json | jq .stdout_json` can never disagree about what
/// `stdout_json` means.
pub fn implicit_outputs(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    wanted: &[String],
    declared: &[String],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let asked = |key: &str| wanted.iter().any(|w| w == key) && !declared.iter().any(|d| d == key);
    for (key, raw) in [(OUT_STDOUT, stdout), (OUT_STDERR, stderr)] {
        if asked(key) {
            out.push((key.to_string(), implicit_stream_value(raw)));
        }
    }
    // `stdout_json` exists only when the *whole* of stdout parses. A
    // half-JSON value would be worse than none: a downstream path query
    // would silently miss instead of the job saying what happened.
    if asked(OUT_STDOUT_JSON)
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        && !parsed.is_null()
    {
        out.push((
            OUT_STDOUT_JSON.to_string(),
            implicit_stream_value(&parsed.to_string()),
        ));
    }
    // `exit_code` is free — it is already in the job summary — so it is
    // published whenever it is known rather than only when it was asked
    // for. Nothing was read back across the mesh to produce it.
    if let Some(code) = exit_code
        && !declared.iter().any(|d| d == OUT_EXIT_CODE)
    {
        out.push((OUT_EXIT_CODE.to_string(), code.to_string()));
    }
    out
}

// ---------------------------------------------------------------------------
// Object references
// ---------------------------------------------------------------------------

/// Format an object reference for a record.
pub fn object_ref(id: &mesh_proto::ObjectId, size: u64) -> String {
    format!("{OBJECT_REF_PREFIX}{}#{size}", id.to_display_string())
}

/// Read back what [`object_ref`] wrote.
pub fn parse_object_ref(value: &str) -> Option<(mesh_proto::ObjectId, u64)> {
    let rest = value.strip_prefix(OBJECT_REF_PREFIX)?;
    let (id, size) = rest.split_once('#')?;
    Some((mesh_proto::ObjectId::parse(id).ok()?, size.parse().ok()?))
}

/// The output key at the head of a reference tail, ignoring any path after
/// it: `stdout_json.devices[0].hostname` → `stdout_json`.
///
/// One definition, used by the capture path (to know a log is worth
/// streaming back), by the workflow validator (to know the reference is
/// legal), and by the Director's picker. Three different answers to "which
/// output is this" would be three different bugs.
pub fn output_base_key(tail: &str) -> &str {
    let end = tail.find(['.', '[']).unwrap_or(tail.len());
    &tail[..end]
}

// ---------------------------------------------------------------------------
// Naming a fetched output (R3 P2)
// ---------------------------------------------------------------------------

/// A filename for an item's output: **the name it came in with**.
///
/// This used to fold the item's whole path into one string, replace every
/// character that was not `[A-Za-z0-9._-]`, and cut the result at 100
/// characters. Pointed at a media library that produced, from
/// `/srv/media/Travel Man/S06E05 Christmas Special in Hong Kong (Jon Hamm).mkv`:
///
/// ```text
/// 000001_srv_media_Travel_Man_S06E05_Christmas_Special_in_Hong_Kong
/// ```
///
/// — the spaces and brackets gone, the whole path in the name, and the
/// truncation having eaten `(Jon Hamm)` **and the `.mkv` extension with
/// it**, so the file no longer opened by double-clicking it. A person who
/// asked for their files back did not get their files back.
///
/// Now: the basename, as it was, with only the characters a filename
/// genuinely cannot hold replaced. Spaces, brackets and accents are fine on
/// every filesystem meshware runs on and are none of our business.
/// Collisions — two items in different directories with the same basename —
/// get ` (2)`, ` (3)` before the extension, the same way a browser download
/// does, because the alternative is one item silently overwriting another.
pub fn fetch_filename(ordinal: usize, item: &str, taken: &mut HashSet<String>) -> String {
    // The item is usually a path. Its *name* is what a person recognises.
    let base = item
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(item);
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            // `/` cannot appear, and a control character in a filename is a
            // trap for every tool downstream. Everything else stays.
            '/' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    // An item that is not a path at all, or whose name was entirely
    // unusable, still needs a name: fall back to its position.
    let base = if cleaned.is_empty() {
        format!("{ordinal:06}")
    } else {
        cleaned
    };

    // Split once so truncation and de-duplication both keep the extension.
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 && base.len() - i <= 12 => (&base[..i], &base[i..]),
        _ => (base.as_str(), ""),
    };
    // Leave room for a suffix and stay inside the 255-byte limit every
    // common filesystem has. Truncating on a char boundary, not a byte.
    let stem: String = stem.chars().take(180).collect();

    let mut candidate = format!("{stem}{ext}");
    let mut n = 2;
    while !taken.insert(candidate.clone()) {
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    candidate
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_fetched_output_keeps_the_name_it_came_in_with() {
        let mut taken = HashSet::new();
        // The case that lost a user's files: long name, spaces, brackets.
        assert_eq!(
            fetch_filename(
                1,
                "/home/me/Videos/Travel Man/S06E05 Christmas Special in Hong Kong (Jon Hamm).mkv",
                &mut taken
            ),
            "S06E05 Christmas Special in Hong Kong (Jon Hamm).mkv"
        );
        // Ordinary names are untouched.
        assert_eq!(
            fetch_filename(0, "/srv/media/clip.mp4", &mut taken),
            "clip.mp4"
        );
        // Two items with the same basename in different directories must
        // not overwrite one another.
        assert_eq!(fetch_filename(2, "/a/report.csv", &mut taken), "report.csv");
        assert_eq!(
            fetch_filename(3, "/b/report.csv", &mut taken),
            "report (2).csv"
        );
        assert_eq!(
            fetch_filename(4, "/c/report.csv", &mut taken),
            "report (3).csv"
        );
        // An item that is not a path is still a usable name.
        assert_eq!(fetch_filename(5, "42", &mut taken), "42");
        // Nothing usable at all: fall back to the position rather than
        // writing a file called "".
        assert_eq!(fetch_filename(6, "/", &mut taken), "000006");
        // A very long stem is cut, and the extension survives the cut —
        // the specific failure of the old implementation.
        let long = format!("/x/{}.mkv", "a".repeat(400));
        let got = fetch_filename(7, &long, &mut taken);
        assert!(got.ends_with(".mkv"), "{got}");
        assert!(got.chars().count() <= 200, "{}", got.chars().count());
        // Control characters cannot reach the filesystem.
        assert_eq!(
            fetch_filename(8, "/x/we\u{7}ird.txt", &mut taken),
            "we_ird.txt"
        );
    }

    #[test]
    fn markers_publish_only_declared_keys() {
        let stdout = "building…\n::meshware-output:: sha=abc123\n::meshware-output:: leaked=x\n";
        let declared = vec!["sha".to_string()];
        assert_eq!(
            parse_outputs(stdout, &declared),
            vec![("sha".to_string(), "abc123".to_string())]
        );
    }

    #[test]
    fn a_json_marker_publishes_several_at_once() {
        let stdout = r#"::meshware-output:: {"sha": "abc", "count": 7}"#;
        let declared = vec!["sha".to_string(), "count".to_string()];
        assert_eq!(
            parse_outputs(stdout, &declared),
            vec![
                ("sha".to_string(), "abc".to_string()),
                ("count".to_string(), "7".to_string())
            ]
        );
    }

    #[test]
    fn a_later_marker_wins() {
        let stdout = "::meshware-output:: sha=first\n::meshware-output:: sha=second\n";
        assert_eq!(
            parse_outputs(stdout, &["sha".to_string()]),
            vec![("sha".to_string(), "second".to_string())]
        );
    }

    #[test]
    fn object_refs_round_trip() {
        let id = mesh_proto::ObjectId([7u8; 32]);
        let text = object_ref(&id, 1_048_576);
        assert_eq!(parse_object_ref(&text), Some((id, 1_048_576)));
        assert_eq!(parse_object_ref("abc123"), None);
    }

    /// Nothing asked for writes nothing on the environment — the property
    /// that keeps an ordinary `meshnode run` free of M19 entirely.
    #[test]
    fn wanting_nothing_costs_nothing() {
        assert!(Wanted::none().to_env().is_empty());
        assert_eq!(Wanted::from_env(&[]), Wanted::none());
    }

    #[test]
    fn wanted_round_trips_through_the_environment() {
        let want = Wanted::keys(["sha", "count"]);
        let env = want.to_env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, mesh_proto::OUT_WANT);
        assert_eq!(Wanted::from_env(&env), want);
        assert_eq!(
            want.declared(),
            vec!["sha".to_string(), "count".to_string()]
        );
        assert!(want.contains(OUT_STDOUT_JSON));
    }

    /// `stdout_json` is absent — not empty, not null — when stdout is prose.
    #[test]
    fn stdout_json_is_absent_when_stdout_is_not_json() {
        let wanted: Vec<String> = IMPLICIT_OUTPUTS.iter().map(|k| (*k).to_string()).collect();
        let out = implicit_outputs("hello there", "", Some(0), &wanted, &[]);
        assert!(out.iter().any(|(k, _)| k == OUT_STDOUT));
        assert!(!out.iter().any(|(k, _)| k == OUT_STDOUT_JSON));

        let out = implicit_outputs(r#"{"a":1}"#, "", Some(0), &wanted, &[]);
        assert_eq!(
            out.iter()
                .find(|(k, _)| k == OUT_STDOUT_JSON)
                .map(|(_, v)| v.as_str()),
            Some(r#"{"a":1}"#)
        );
    }
}
