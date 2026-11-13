//! Turning a job's streams into an answer (M19 §5.3–§5.5).
//!
//! Three things live here, and they are deliberately small because the
//! milestone is a *promotion*, not an invention: the output convention it
//! stands on is [`crate::outputs`], moved down from the workflow run loop
//! so every job reaches it.
//!
//! - [`RowCapture`] tees a job's stdout/stderr as they stream past on
//!   their way to the client, and turns them into a [`ResultRow`]. A tee
//!   rather than a re-fetch: the bytes are already crossing this node, so
//!   `--json` costs the same as the plain run.
//! - [`Expectation`] is `--expect`. It is **not a new expression
//!   language** — it names a task from the built-in catalogue and hands it
//!   two strings. Every comparison, every error message and every
//!   explanation comes from `tasks.rs`, which already runs on the
//!   coordinating node with no dispatch (§5.5).
//! - [`preview_of`] gives a result set the one-line header a person needs
//!   to recognise which fan-out they are looking at.

use mesh_proto::{JobId, JobState, NodeId, ResultRow};

use crate::outputs::{self, Wanted};

/// Most log bytes scanned for outputs. A job that prints a gigabyte is not
/// going to have its answer at byte 900,000,000, and scanning to find out
/// would cost every fan-out target the same.
pub const MAX_CAPTURE: usize = 4 * 1024 * 1024;

/// A live tee of one job's streams into a row.
///
/// Constructed only when the caller asked for outputs, so a job nobody
/// asked about allocates nothing and copies nothing.
#[derive(Debug)]
pub struct RowCapture {
    wanted: Wanted,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl RowCapture {
    /// `None` when the caller asked for nothing — the common case, and the
    /// reason `meshnode run 'uname -a'` is exactly as fast as it was.
    pub fn new(wanted: &Wanted) -> Option<RowCapture> {
        (!wanted.is_empty()).then(|| RowCapture {
            wanted: wanted.clone(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    /// Feed one log chunk on its way past. `stream`: 1 = stdout, 2 = stderr.
    pub fn chunk(&mut self, stream: u8, data: &[u8]) {
        let sink = match stream {
            1 => &mut self.stdout,
            2 => &mut self.stderr,
            _ => return,
        };
        if sink.len() < MAX_CAPTURE {
            sink.extend_from_slice(data);
        }
    }

    /// Build the row.
    ///
    /// `spill` is handed any declared value over the inline bound and
    /// returns the reference to record instead — an M9 ephemeral object, so
    /// a build log never travels as a value. Returning `None` keeps the
    /// value inline, which is what happens when the object store refuses:
    /// a large answer is better than no answer.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        self,
        job_id: JobId,
        node: NodeId,
        node_name: String,
        state: JobState,
        exit_code: Option<i32>,
        finished_at_micros: Option<u64>,
        spill: impl Fn(&str, &str) -> Option<String>,
    ) -> ResultRow {
        let stdout = String::from_utf8_lossy(&self.stdout);
        let stderr = String::from_utf8_lossy(&self.stderr);
        let declared = self.wanted.declared();

        // Declared outputs lead: what the caller named is what they will
        // look for first, and the four that arrive for free belong after.
        let mut out: Vec<(String, String)> = outputs::parse_outputs(&stdout, &declared)
            .into_iter()
            .map(|(key, value)| {
                if value.len() <= outputs::INLINE_OUTPUT_MAX {
                    return (key, value);
                }
                let reference = spill(&key, &value);
                (key, reference.unwrap_or(value))
            })
            .collect();
        out.extend(outputs::implicit_outputs(
            &stdout,
            &stderr,
            exit_code,
            self.wanted.as_slice(),
            &declared,
        ));

        ResultRow {
            job_id,
            node,
            node_name,
            state,
            exit_code,
            outputs: out,
            finished_at_micros,
            expect_passed: None,
            expect_detail: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// --expect (§5.5)
// ---------------------------------------------------------------------------

/// One assertion, evaluated row by row on the origin.
///
/// **Overturned: a bespoke query/assertion DSL over results.** The archive
/// already speaks SQL and a second dialect in the daemon is two dialects to
/// keep in step. What people want from `--expect` is a guard in a pipeline,
/// and the guard already exists: this is a call-syntax front end over the
/// built-in task catalogue, and every semantic in it belongs to a task.
///
/// ```console
/// --expect 'contains(stdout, "permitrootlogin no")'
/// --expect 'equals(exit_code, "0")'
/// --expect 'json.query(stdout_json, "$.total")'
/// ```
///
/// The first form is sugar for `logic.check` with an operator; the third is
/// the escape hatch — a dotted name **is** a task id, and the arguments map
/// onto its first two inputs positionally. Nothing is added to the
/// catalogue and nothing is interpreted here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expectation {
    /// The original text, echoed in the failure report.
    pub source: String,
    /// The built-in task that decides it.
    pub task: String,
    /// The output key whose value becomes the task's first input.
    pub key: String,
    /// The task's first input name.
    pub value_input: String,
    /// Literal inputs, already named.
    pub fixed: Vec<(String, String)>,
}

/// Aliases that read like an assertion and resolve to a `logic.check`
/// operator. The list is exactly `logic.check`'s own operator choice, so it
/// can never drift from what the task supports.
const OPERATORS: &[(&str, &str)] = &[
    ("equals", "equals"),
    ("eq", "equals"),
    ("not_equals", "does not equal"),
    ("ne", "does not equal"),
    ("contains", "contains"),
    ("not_contains", "does not contain"),
    ("starts_with", "starts with"),
    ("ends_with", "ends with"),
    ("matches", "matches"),
    ("empty", "is empty"),
    ("not_empty", "is not empty"),
    ("one_of", "is one of"),
    ("gt", "greater than"),
    ("gte", "at least"),
    ("lt", "less than"),
    ("lte", "at most"),
];

impl Expectation {
    /// Parse `fn(key)` or `fn(key, "literal")`.
    ///
    /// Every refusal names what is wrong and what to do about it, the way
    /// M10's validator set the standard — an assertion that fails to parse
    /// silently would turn a guard into a no-op, which is worse than no
    /// guard at all.
    pub fn parse(source: &str) -> std::result::Result<Expectation, String> {
        let text = source.trim();
        let Some(open) = text.find('(') else {
            return Err(format!(
                "--expect wants a call: {}. Got {text:?}",
                Expectation::usage()
            ));
        };
        if !text.ends_with(')') {
            return Err(format!("--expect {text:?} is missing its closing `)`"));
        }
        let name = text[..open].trim().to_ascii_lowercase();
        let args = split_args(&text[open + 1..text.len() - 1]);
        if args.is_empty() || args[0].is_empty() {
            return Err(format!(
                "--expect {name}(…) needs an output key first, e.g. \
                 {name}(stdout, \"…\") — the four every job publishes are \
                 stdout, stdout_json, stderr and exit_code"
            ));
        }
        if args.len() > 2 {
            return Err(format!(
                "--expect {name}(…) takes at most two arguments, got {}",
                args.len()
            ));
        }
        let key = args[0].clone();
        let second = args.get(1).cloned().unwrap_or_default();

        // A dotted name is a task id, verbatim from the catalogue.
        if name.contains('.') {
            let Some(def) = crate::tasks::find(&name) else {
                // The catalogue used to be listed by `meshnode workflow
                // tasks`, which went with the workflow engine. `--expect`
                // is its remaining caller, so the suggestion has to come
                // from here: the ids sharing this one's prefix, or the
                // prefixes themselves when even that missed.
                let prefix = name.split('.').next().unwrap_or_default();
                let mut near: Vec<&str> = crate::tasks::TASKS
                    .iter()
                    .map(|t| t.id)
                    .filter(|id| id.starts_with(&format!("{prefix}.")))
                    .collect();
                if near.is_empty() {
                    near = crate::tasks::TASKS
                        .iter()
                        .filter_map(|t| t.id.split('.').next())
                        .collect();
                    near.sort_unstable();
                    near.dedup();
                    return Err(format!(
                        "--expect names no built-in task {name:?}. \
                         Known groups: {}",
                        near.join(", ")
                    ));
                }
                near.sort_unstable();
                return Err(format!(
                    "--expect names no built-in task {name:?}. \
                     In {prefix}: {}",
                    near.join(", ")
                ));
            };
            let mut inputs = def.inputs.iter();
            let Some(first) = inputs.next() else {
                return Err(format!("--expect {name}(…) takes no inputs"));
            };
            let mut fixed = Vec::new();
            if !second.is_empty()
                && let Some(next) = inputs.next()
            {
                fixed.push((next.name.to_string(), second));
            }
            return Ok(Expectation {
                source: text.to_string(),
                task: name,
                key,
                value_input: first.name.to_string(),
                fixed,
            });
        }

        let Some((_, operator)) = OPERATORS.iter().find(|(alias, _)| *alias == name) else {
            return Err(format!(
                "--expect does not know {name:?}. {}",
                Expectation::usage()
            ));
        };
        Ok(Expectation {
            source: text.to_string(),
            task: "logic.check".to_string(),
            key,
            value_input: "value".to_string(),
            fixed: vec![
                ("operator".to_string(), (*operator).to_string()),
                ("compare_to".to_string(), second),
            ],
        })
    }

    fn usage() -> String {
        let names: Vec<&str> = OPERATORS.iter().map(|(alias, _)| *alias).collect();
        format!(
            "one of {}(key[, \"value\"]), or any built-in task id such as \
             json.query(stdout_json, \"$.total\")",
            names.join(" / ")
        )
    }

    /// Decide one row, recording the verdict and the evaluator's own
    /// sentence about it on the row itself.
    pub fn judge(&self, row: &mut ResultRow) {
        let value = row.output(&self.key).unwrap_or("").to_string();
        let mut inputs = vec![(self.value_input.clone(), value)];
        inputs.extend(self.fixed.iter().cloned());
        match crate::tasks::run(&self.task, &inputs) {
            Ok(out) => {
                let value = out
                    .iter()
                    .find(|(k, _)| k == "value")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                row.expect_passed = Some(crate::tasks::truthy(value));
                row.expect_detail = out
                    .iter()
                    .find(|(k, _)| k == "explain")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| format!("{} = {value}", self.key));
            }
            // A task that refuses to run is a failed assertion, not a
            // passed one: an unevaluable guard must never read as a pass.
            Err(why) => {
                row.expect_passed = Some(false);
                row.expect_detail = why;
            }
        }
    }
}

/// Split `a, "b, c"` into `["a", "b, c"]`, honouring quotes.
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, ',') => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            (None, c) => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    if out.len() == 1 && out[0].is_empty() {
        return Vec::new();
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn row(outputs: &[(&str, &str)]) -> ResultRow {
        ResultRow {
            job_id: JobId([1u8; 16]),
            node: NodeId([2u8; 32]),
            node_name: "hermes".into(),
            state: JobState::Succeeded,
            exit_code: Some(0),
            outputs: outputs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            finished_at_micros: None,
            expect_passed: None,
            expect_detail: String::new(),
        }
    }

    #[test]
    fn an_alias_resolves_to_a_logic_check_operator() {
        let e = Expectation::parse(r#"contains(stdout, "permitrootlogin no")"#).unwrap();
        assert_eq!(e.task, "logic.check");
        assert_eq!(e.key, "stdout");
        assert_eq!(
            e.fixed,
            vec![
                ("operator".to_string(), "contains".to_string()),
                ("compare_to".to_string(), "permitrootlogin no".to_string())
            ]
        );
    }

    #[test]
    fn it_judges_a_row_and_says_why() {
        let e = Expectation::parse(r#"contains(stdout, "permitrootlogin no")"#).unwrap();
        let mut yes = row(&[("stdout", "permitrootlogin no")]);
        let mut no = row(&[("stdout", "permitrootlogin yes")]);
        e.judge(&mut yes);
        e.judge(&mut no);
        assert_eq!(yes.expect_passed, Some(true));
        assert_eq!(no.expect_passed, Some(false));
        assert!(!no.expect_detail.is_empty(), "a failure must say why");
    }

    /// Comparison is case-sensitive, because `logic.check` is — which is
    /// the point of borrowing it rather than writing a comparator here.
    /// `matches(key, "glob")` is the escape hatch when case is not known.
    #[test]
    fn comparison_is_the_tasks_comparison_and_not_a_new_one() {
        let e = Expectation::parse(r#"contains(stdout, "permitrootlogin no")"#).unwrap();
        let mut r = row(&[("stdout", "PermitRootLogin no")]);
        e.judge(&mut r);
        assert_eq!(r.expect_passed, Some(false));
    }

    /// A missing output is a failure, never a silent pass — the whole
    /// point of a guard in a pipeline.
    #[test]
    fn a_missing_output_fails_rather_than_passing() {
        let e = Expectation::parse(r#"contains(stdout_json, "x")"#).unwrap();
        let mut r = row(&[("stdout", "prose, not json")]);
        e.judge(&mut r);
        assert_eq!(r.expect_passed, Some(false));
    }

    #[test]
    fn a_dotted_name_is_a_catalogue_task() {
        let e = Expectation::parse(r#"json.query(stdout_json, "$.total")"#).unwrap();
        assert_eq!(e.task, "json.query");
        assert_eq!(e.value_input, "json");
        assert_eq!(e.fixed[0].0, "path");
    }

    #[test]
    fn nonsense_names_what_is_wrong() {
        let why = Expectation::parse("wibble(stdout)").unwrap_err();
        assert!(why.contains("wibble"), "{why}");
        assert!(
            why.contains("contains"),
            "the message must list the aliases"
        );

        let why = Expectation::parse("contains stdout").unwrap_err();
        assert!(why.contains("call"), "{why}");
    }

    #[test]
    fn quoted_commas_survive_splitting() {
        assert_eq!(split_args(r#"stdout, "a, b""#), vec!["stdout", "a, b"]);
        assert_eq!(split_args("stdout"), vec!["stdout"]);
        assert!(split_args("").is_empty());
    }
}
