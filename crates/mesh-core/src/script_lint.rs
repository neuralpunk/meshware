//! Structural checks for an inline script (M10 §4.7).
//!
//! **This is not a Python implementation and does not pretend to be one.**
//! It is a tokenizer that knows about quotes, brackets, comments and
//! indentation, and it reports the four mistakes that are worth catching
//! *before* a step is dispatched to a node:
//!
//!   * a bracket that is never closed, or closed with the wrong one;
//!   * a string that is never closed;
//!   * a `:` that opens a block with nothing indented under it;
//!   * tabs and spaces mixed in one file's indentation.
//!
//! Every one of those is a `SyntaxError` at run time — three minutes and a
//! scheduler decision later, with the traceback in a log somebody has to go
//! and open. Catching them here means they arrive as a badge on the node,
//! from the same validator every other badge comes from, so what the canvas
//! says and what publish accepts cannot drift (§4.4.2 F).
//!
//! Everything else — a name that is not defined, a type that will not add —
//! is deliberately out of scope. A linter that is wrong occasionally is
//! worse than no linter at all, because people learn to ignore it.

use mesh_proto::Runtime;

/// One problem, anchored to a line so an editor can put a mark on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptIssue {
    /// 1-based, to match every editor and every traceback.
    pub line: u32,
    pub message: String,
}

impl ScriptIssue {
    fn at(line: usize, message: impl Into<String>) -> ScriptIssue {
        ScriptIssue {
            line: line as u32 + 1,
            message: message.into(),
        }
    }
}

/// Check a script body. An empty list means nothing structural is wrong —
/// it does not mean the script is correct.
pub fn lint(runtime: Runtime, source: &str) -> Vec<ScriptIssue> {
    let mut issues = brackets_and_strings(runtime, source);
    if runtime == Runtime::Python {
        issues.extend(python_blocks(source));
        issues.extend(indentation(source));
    }
    issues.sort_by_key(|i| i.line);
    issues.truncate(8);
    issues
}

/// True when this line's code (comments and strings removed) is inside a
/// continuation — used so block checks do not fire on wrapped expressions.
struct Scan {
    /// Depth of unclosed brackets at the end of each line.
    depth_after: Vec<i32>,
    /// The code on each line, with strings blanked and comments removed.
    bare: Vec<String>,
    issues: Vec<ScriptIssue>,
}

fn brackets_and_strings(runtime: Runtime, source: &str) -> Vec<ScriptIssue> {
    scan(runtime, source).issues
}

/// One pass over the source that both blanks strings and tracks brackets.
/// They have to be done together: a `)` inside a string closes nothing, and
/// a `#` inside a string does not start a comment.
fn scan(runtime: Runtime, source: &str) -> Scan {
    let python = runtime == Runtime::Python;
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut issues: Vec<ScriptIssue> = Vec::new();
    let mut depth_after: Vec<i32> = Vec::new();
    let mut bare: Vec<String> = Vec::new();
    // A triple-quoted string spans lines, so it lives outside the loop.
    let mut triple: Option<(&'static str, usize)> = None;

    for (n, raw) in source.lines().enumerate() {
        let chars: Vec<char> = raw.chars().collect();
        let mut out = String::with_capacity(chars.len());
        let mut i = 0;
        while i < chars.len() {
            // Finish an open triple-quoted string first.
            if let Some((marker, _)) = triple {
                let rest: String = chars[i..].iter().collect();
                match rest.find(marker) {
                    Some(at) => {
                        i += at + marker.len();
                        triple = None;
                        out.push(' ');
                    }
                    None => {
                        i = chars.len();
                    }
                }
                continue;
            }
            let c = chars[i];
            // Comments run to end of line in both runtimes.
            if c == '#' {
                break;
            }
            if c == '"' || c == '\'' {
                let marker =
                    if python && chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c) {
                        if c == '"' {
                            Some("\"\"\"")
                        } else {
                            Some("'''")
                        }
                    } else {
                        None
                    };
                if let Some(marker) = marker {
                    i += 3;
                    let rest: String = chars[i..].iter().collect();
                    match rest.find(marker) {
                        Some(at) => i += at + 3,
                        None => {
                            triple = Some((marker, n));
                            i = chars.len();
                        }
                    }
                    out.push(' ');
                    continue;
                }
                // A single-quoted string, which must close on this line.
                let quote = c;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == quote {
                        closed = true;
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    issues.push(ScriptIssue::at(
                        n,
                        format!("this {quote} string is never closed"),
                    ));
                }
                out.push(' ');
                continue;
            }
            match c {
                '(' | '[' | '{' => stack.push((c, n)),
                ')' | ']' | '}' => {
                    let want = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    match stack.pop() {
                        Some((open, _)) if open == want => {}
                        Some((open, at)) => issues.push(ScriptIssue::at(
                            n,
                            format!(
                                "{c} closes the {open} opened on line {} — they do not match",
                                at + 1
                            ),
                        )),
                        None => issues.push(ScriptIssue::at(n, format!("{c} closes nothing"))),
                    }
                }
                _ => {}
            }
            out.push(c);
            i += 1;
        }
        bare.push(out);
        depth_after.push(stack.len() as i32);
    }

    if let Some((marker, at)) = triple {
        issues.push(ScriptIssue::at(
            at,
            format!("this {marker} string is never closed"),
        ));
    }
    for (open, at) in stack {
        issues.push(ScriptIssue::at(at, format!("this {open} is never closed")));
    }
    Scan {
        depth_after,
        bare,
        issues,
    }
}

/// A `:` that opens a block needs something indented under it. This is the
/// single most common way a half-finished Python step reaches a node.
fn python_blocks(source: &str) -> Vec<ScriptIssue> {
    let scan = scan(Runtime::Python, source);
    let mut issues = Vec::new();
    for (n, code) in scan.bare.iter().enumerate() {
        // Only whole statements: a `:` inside an unclosed bracket is a dict
        // or an annotation, not a block.
        if scan.depth_after.get(n).copied().unwrap_or(0) != 0 {
            continue;
        }
        let trimmed = code.trim_end();
        if !trimmed.ends_with(':') {
            continue;
        }
        let opener = trimmed.trim_start();
        const KEYWORDS: [&str; 11] = [
            "if ", "elif ", "else", "for ", "while ", "def ", "class ", "try", "except", "finally",
            "with ",
        ];
        if !KEYWORDS.iter().any(|k| opener.starts_with(k)) {
            continue;
        }
        let indent = code.len() - code.trim_start().len();
        let body = scan
            .bare
            .iter()
            .skip(n + 1)
            .find(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len());
        match body {
            Some(next) if next > indent => {}
            _ => issues.push(ScriptIssue::at(
                n,
                format!(
                    "`{}` opens a block, but nothing is indented under it",
                    opener.split_whitespace().next().unwrap_or("this")
                ),
            )),
        }
    }
    issues
}

/// Tabs and spaces in one file's indentation. Python 3 refuses it, and the
/// failure is invisible in most editors — which is exactly why it is worth
/// a line here.
fn indentation(source: &str) -> Vec<ScriptIssue> {
    let mut saw_tab: Option<usize> = None;
    let mut saw_space: Option<usize> = None;
    for (n, line) in source.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if indent.contains('\t') && indent.contains(' ') {
            return vec![ScriptIssue::at(
                n,
                "this line indents with both tabs and spaces",
            )];
        }
        if indent.contains('\t') {
            saw_tab.get_or_insert(n);
        } else if !indent.is_empty() {
            saw_space.get_or_insert(n);
        }
    }
    match (saw_tab, saw_space) {
        (Some(tab), Some(space)) => vec![ScriptIssue::at(
            tab.max(space),
            format!(
                "line {} indents with tabs and line {} with spaces — Python needs one or the other",
                tab + 1,
                space + 1
            ),
        )],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py(source: &str) -> Vec<String> {
        lint(Runtime::Python, source)
            .into_iter()
            .map(|i| format!("{}: {}", i.line, i.message))
            .collect()
    }

    #[test]
    fn a_correct_script_reports_nothing() {
        assert!(
            py("import json\n\ndef main():\n    data = {'a': [1, 2]}\n    print(json.dumps(data))\n\nmain()\n")
                .is_empty()
        );
    }

    #[test]
    fn an_unclosed_bracket_names_the_line_it_opened_on() {
        let out = py("print(\n    1\n");
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].starts_with("1: "), "{out:?}");
        assert!(out[0].contains("never closed"), "{out:?}");
    }

    #[test]
    fn a_mismatched_bracket_says_which_one_it_was_meant_to_close() {
        let out = py("x = [1, 2)\n");
        assert!(out[0].contains("do not match"), "{out:?}");
    }

    #[test]
    fn an_unclosed_string_is_caught_on_its_own_line() {
        let out = py("name = 'mia\nprint(name)\n");
        assert!(out[0].starts_with("1: "), "{out:?}");
        assert!(out[0].contains("never closed"), "{out:?}");
    }

    /// A docstring spans lines legitimately; an unterminated one does not.
    #[test]
    fn triple_quoted_strings_span_lines_but_still_have_to_close() {
        assert!(py("\"\"\"\nnotes\n\"\"\"\nprint(1)\n").is_empty());
        let out = py("\"\"\"\nnotes\nprint(1)\n");
        assert!(out[0].contains("never closed"), "{out:?}");
    }

    /// Brackets and comments inside strings are text, not syntax.
    #[test]
    fn syntax_inside_a_string_is_not_syntax() {
        assert!(py("print('a ) b # c')\n").is_empty());
        assert!(py("# a stray ) in a comment\nprint(1)\n").is_empty());
    }

    #[test]
    fn a_block_with_no_body_is_the_half_finished_step_this_exists_for() {
        let out = py("for x in range(3):\n");
        assert!(out[0].contains("nothing is indented"), "{out:?}");
        assert!(py("for x in range(3):\n    print(x)\n").is_empty());
    }

    /// A `:` inside brackets is a dict or an annotation, not a block.
    #[test]
    fn a_colon_inside_brackets_does_not_open_a_block() {
        assert!(py("data = {\n    'a': 1,\n}\n").is_empty());
        assert!(py("def f(x: int) -> int:\n    return x\n").is_empty());
    }

    #[test]
    fn mixed_indentation_is_reported_once() {
        let out = py("if True:\n\tprint(1)\nif False:\n    print(2)\n");
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("tabs"), "{out:?}");
    }

    /// Bash gets bracket and quote checking but none of Python's block or
    /// indentation rules, because none of them are true of bash.
    #[test]
    fn bash_is_checked_for_quotes_but_not_for_indentation() {
        let out = lint(Runtime::Bash, "echo 'unterminated\n");
        assert_eq!(out.len(), 1);
        assert!(lint(Runtime::Bash, "if true; then\necho hi\nfi\n").is_empty());
        assert!(lint(Runtime::Bash, "\tone\n  two\n").is_empty());
    }

    /// A wall of errors from one missing bracket helps nobody.
    #[test]
    fn the_list_is_capped() {
        let source = (0..40).map(|i| format!("x{i} = (\n")).collect::<String>();
        assert!(py(&source).len() <= 8);
    }
}
