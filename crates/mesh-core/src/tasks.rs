//! Built-in tasks: the steps that need no node (M10 §4.6).
//!
//! A workflow spends most of its edges moving small values around —
//! uppercase this, base64 that, pull a field out of some JSON, decide
//! whether a number crossed a threshold. Before this module every one of
//! those was `run: { command: "..." }`, which meant shelling out to a mesh
//! node, which meant a scheduler decision, a job record, a log stream and a
//! round trip, for `echo` with extra steps. Worse, it meant the workflow
//! only worked on nodes that happened to have `jq`, or `base64`, or the
//! GNU flavour of `sed`.
//!
//! A built-in task is a **pure function from strings to strings**, defined
//! here, evaluated by the run loop on the coordinating node itself. No
//! dispatch, no executor, no job. That has three consequences worth
//! stating, because they are the whole design:
//!
//!   * **It cannot touch the world.** No filesystem, no network, no
//!     process spawn, no clock beyond `now_micros`. A task that wanted any
//!     of those would be a `command` or a `service`, and the boundary is
//!     kept sharp on purpose: this is the part of a workflow that is safe
//!     to run anywhere, including in a preview.
//!   * **It is deterministic**, with exactly two advertised exceptions
//!     (`time.*` and `random.*`), both of which say so in their summary.
//!   * **It is total.** Every failure is a returned `Err(String)` that
//!     names what was wrong with which input. A task never panics, so a
//!     malformed template in step 40 fails step 40 instead of the daemon.
//!
//! The catalogue is data, not code: [`TASKS`] is a flat table the Director
//! renders directly into its palette, so adding a task is one table entry
//! plus one match arm, and the UI picks it up with no further work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Catalogue types
// ---------------------------------------------------------------------------

/// How the Director should draw a field for one task input.
///
/// This is presentation metadata and nothing more — every value crosses the
/// wire as a string, and every task parses what it needs. Keeping the
/// engine string-typed is what lets `{{steps.x.outputs.y}}` be substituted
/// into *any* input without the template system needing a type checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    /// One line.
    Text,
    /// A code / prose box.
    Multiline,
    /// Numeric, but still carried as text (it may be a template).
    Number,
    /// `true` / `false`.
    Toggle,
    /// One of [`TaskInput::choices`].
    Choice,
}

impl FieldKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Multiline => "multiline",
            FieldKind::Number => "number",
            FieldKind::Toggle => "toggle",
            FieldKind::Choice => "choice",
        }
    }
}

/// What kind of value a field holds.
///
/// Everything crosses the wire as a string — that is what lets
/// `{{steps.x.outputs.y}}` be substituted into *any* input without the
/// template system needing a type checker. `DataShape` is the layer above
/// that: it says what the string is *meant* to be, so the Director can show
/// it on the field, pick the right editor, and warn when somebody pipes a
/// list into a slot that will try to parse JSON.
///
/// It is a hint, never a gate. A task that needs its input to really be
/// JSON parses it and fails with a message; the shape is what stops that
/// failure from being a surprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataShape {
    /// Any string.
    Text,
    /// Parses as a number.
    Number,
    /// Truthy or falsy — the thing an edge guard reads.
    Boolean,
    /// A JSON document.
    Json,
    /// Newline-separated items. The list convention every `list.*` task
    /// hands on and expects to be handed.
    List,
}

impl DataShape {
    pub fn as_str(&self) -> &'static str {
        match self {
            DataShape::Text => "text",
            DataShape::Number => "number",
            DataShape::Boolean => "boolean",
            DataShape::Json => "json",
            DataShape::List => "list",
        }
    }
}

/// One named input to a task.
#[derive(Debug, Clone)]
pub struct TaskInput {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    /// Required inputs are the ones validation refuses to publish without.
    pub required: bool,
    /// Used when the input is absent. Also what the Director prefills.
    pub default: &'static str,
    pub choices: &'static [&'static str],
    pub help: &'static str,
    /// What this field is meant to hold.
    pub shape: DataShape,
    /// Fold this one away behind "More options". A form that opens with ten
    /// fields teaches people the form is somebody else's problem; a
    /// separator override is not what anybody came here to set.
    pub advanced: bool,
}

/// A task, as the palette lists it and the validator checks it.
#[derive(Debug, Clone)]
pub struct TaskDef {
    /// Stable id, `category.verb`. This is what a workflow file names, so
    /// it is API: renaming one breaks published definitions.
    pub id: &'static str,
    pub category: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub inputs: &'static [TaskInput],
    /// Output keys this task publishes. Downstream steps reference them as
    /// `{{steps.<id>.outputs.<key>}}`, exactly like a command's declared
    /// outputs — there is no second mechanism.
    pub outputs: &'static [&'static str],
    /// The shape of the primary `value` output — what the next step is
    /// really being handed.
    pub produces: DataShape,
    /// Words the palette search matches beyond the title.
    pub keywords: &'static str,
}

/// The categories, in the order the palette shows them. Ordering is
/// editorial: the things a first workflow needs are near the top.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("Flow", "Constants, guards, and stopping on purpose"),
    ("Text", "Reshape a string"),
    ("Number", "Arithmetic and comparison"),
    ("Logic", "Compare, decide, and branch"),
    ("List", "Split, join, sort, pick"),
    ("JSON", "Read and write structured data"),
    ("Data", "Check and convert between shapes"),
    ("Encoding", "Base64, hex, URL, HTML"),
    ("Hash", "Content digests"),
    ("Path", "Filesystem-shaped strings"),
    ("Time", "Clocks and durations"),
    ("Random", "Ids and sampling"),
];

const fn input(
    name: &'static str,
    label: &'static str,
    kind: FieldKind,
    required: bool,
    default: &'static str,
    help: &'static str,
) -> TaskInput {
    TaskInput {
        name,
        label,
        kind,
        required,
        default,
        choices: &[],
        help,
        shape: DataShape::Text,
        advanced: false,
    }
}

/// An input that means something more specific than "a string".
const fn shaped(
    name: &'static str,
    label: &'static str,
    kind: FieldKind,
    required: bool,
    default: &'static str,
    shape: DataShape,
    help: &'static str,
) -> TaskInput {
    let mut i = input(name, label, kind, required, default, help);
    i.shape = shape;
    i
}

/// Fold an input behind "More options".
const fn tucked(mut i: TaskInput) -> TaskInput {
    i.advanced = true;
    i
}

const fn choice(
    name: &'static str,
    label: &'static str,
    default: &'static str,
    choices: &'static [&'static str],
    help: &'static str,
) -> TaskInput {
    TaskInput {
        name,
        label,
        kind: FieldKind::Choice,
        required: false,
        default,
        choices,
        help,
        shape: DataShape::Text,
        advanced: false,
    }
}

/// `text` — the input almost every task in here takes.
const TEXT: TaskInput = input(
    "text",
    "Text",
    FieldKind::Multiline,
    true,
    "",
    "The string to work on. Templates are resolved first.",
);

macro_rules! task {
    ($id:literal, $cat:literal, $title:literal, $summary:literal, $kw:literal,
     [$($in:expr),* $(,)?], [$($out:literal),* $(,)?]) => {
        task!($id, $cat, $title, $summary, $kw, [$($in),*], [$($out),*], DataShape::Text)
    };
    ($id:literal, $cat:literal, $title:literal, $summary:literal, $kw:literal,
     [$($in:expr),* $(,)?], [$($out:literal),* $(,)?], $shape:expr) => {
        TaskDef {
            id: $id,
            category: $cat,
            title: $title,
            summary: $summary,
            inputs: &[$($in),*],
            outputs: &[$($out),*],
            produces: $shape,
            keywords: $kw,
        }
    };
}

/// The catalogue.
///
/// Flat and alphabetical *within* a category, because the palette groups by
/// category and searches across everything. Every entry here must have a
/// matching arm in [`run`]; the test at the bottom of this file enforces
/// that, so a task can never be listed and unimplemented.
pub const TASKS: &[TaskDef] = &[
    // -- Flow ---------------------------------------------------------------
    task!(
        "flow.set",
        "Flow",
        "Set value",
        "Publish a constant (or a template) as this step's output.",
        "constant literal variable assign",
        [input(
            "value",
            "Value",
            FieldKind::Multiline,
            true,
            "",
            "What to publish."
        )],
        ["value"]
    ),
    task!(
        "flow.note",
        "Flow",
        "Note",
        "Does nothing and always succeeds. A label on the canvas.",
        "comment label docs annotation",
        [input(
            "text",
            "Note",
            FieldKind::Multiline,
            false,
            "",
            "Shown on the node."
        )],
        ["value"]
    ),
    task!(
        "flow.fail",
        "Flow",
        "Fail",
        "Always fails, with the message you give it. Drives a `fail` lane on purpose.",
        "error raise throw abort stop",
        [input(
            "message",
            "Message",
            FieldKind::Text,
            false,
            "failed on purpose",
            "Why the run stopped."
        )],
        []
    ),
    task!(
        "flow.assert",
        "Flow",
        "Assert",
        "Succeeds when the condition is truthy, fails otherwise.",
        "check guard require verify precondition",
        [
            shaped(
                "condition",
                "Condition",
                FieldKind::Text,
                true,
                "",
                DataShape::Boolean,
                "Truthy: true, yes, 1, on, or any non-empty string other than false/no/0/off."
            ),
            input(
                "message",
                "Message if false",
                FieldKind::Text,
                false,
                "assertion failed",
                "The failure detail."
            ),
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "flow.default",
        "Flow",
        "Default",
        "Passes the value through, substituting a fallback when it is empty.",
        "fallback coalesce empty null default",
        [
            input(
                "value",
                "Value",
                FieldKind::Text,
                false,
                "",
                "May be empty."
            ),
            input(
                "fallback",
                "Fallback",
                FieldKind::Text,
                true,
                "",
                "Used when the value is empty."
            ),
        ],
        ["value"]
    ),
    task!(
        "flow.pick",
        "Flow",
        "Pick first non-empty",
        "The first of up to four values that is not empty.",
        "coalesce first any fallback",
        [
            input("a", "First", FieldKind::Text, false, "", ""),
            input("b", "Second", FieldKind::Text, false, "", ""),
            input("c", "Third", FieldKind::Text, false, "", ""),
            input("d", "Fourth", FieldKind::Text, false, "", ""),
        ],
        ["value"]
    ),
    task!(
        "flow.merge",
        "Flow",
        "Merge values",
        "Joins up to four values into one string, skipping the empty ones.",
        "concat combine join gather collect",
        [
            input("a", "First", FieldKind::Text, false, "", ""),
            input("b", "Second", FieldKind::Text, false, "", ""),
            input("c", "Third", FieldKind::Text, false, "", ""),
            input("d", "Fourth", FieldKind::Text, false, "", ""),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                " ",
                ""
            )),
        ],
        ["value"]
    ),
    // -- Text ---------------------------------------------------------------
    task!(
        "text.upper",
        "Text",
        "Uppercase",
        "ABC.",
        "uppercase caps capitalize",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.lower",
        "Text",
        "Lowercase",
        "abc.",
        "lowercase downcase",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.title",
        "Text",
        "Title Case",
        "Capitalises the first letter of each word.",
        "titlecase capitalize",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.trim",
        "Text",
        "Trim",
        "Removes whitespace from both ends.",
        "strip whitespace clean",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.trim_start",
        "Text",
        "Trim start",
        "Removes leading whitespace.",
        "ltrim strip left",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.trim_end",
        "Text",
        "Trim end",
        "Removes trailing whitespace.",
        "rtrim strip right",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.length",
        "Text",
        "Length",
        "Character count and byte count. Both, because they differ the moment anyone types an accent.",
        "size count strlen chars bytes",
        [TEXT],
        ["value", "bytes"],
        DataShape::Number
    ),
    task!(
        "text.reverse",
        "Text",
        "Reverse",
        "Reverses the characters.",
        "flip backwards",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.replace",
        "Text",
        "Replace",
        "Replaces every occurrence of a literal substring.",
        "substitute swap sed gsub",
        [
            TEXT,
            input(
                "find",
                "Find",
                FieldKind::Text,
                true,
                "",
                "Literal text, not a pattern."
            ),
            input("replace", "Replace with", FieldKind::Text, false, "", ""),
        ],
        ["value", "count"]
    ),
    task!(
        "text.slice",
        "Text",
        "Slice",
        "A character range. Negative offsets count from the end.",
        "substring cut range chars",
        [
            TEXT,
            input(
                "start",
                "Start",
                FieldKind::Number,
                false,
                "0",
                "0-based, inclusive."
            ),
            input(
                "end",
                "End",
                FieldKind::Number,
                false,
                "",
                "Exclusive. Empty means to the end."
            ),
        ],
        ["value"]
    ),
    task!(
        "text.truncate",
        "Text",
        "Truncate",
        "Caps the length, appending an ellipsis when it had to cut.",
        "shorten ellipsis limit clip",
        [
            TEXT,
            input(
                "limit",
                "Max characters",
                FieldKind::Number,
                false,
                "80",
                ""
            ),
            tucked(input("suffix", "Suffix", FieldKind::Text, false, "…", "")),
        ],
        ["value", "truncated"]
    ),
    task!(
        "text.contains",
        "Text",
        "Contains",
        "Whether a literal substring appears.",
        "includes search find has match",
        [TEXT, input("find", "Find", FieldKind::Text, true, "", "")],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "text.starts_with",
        "Text",
        "Starts with",
        "Prefix test.",
        "prefix begins",
        [
            TEXT,
            input("prefix", "Prefix", FieldKind::Text, true, "", "")
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "text.ends_with",
        "Text",
        "Ends with",
        "Suffix test.",
        "suffix ends",
        [
            TEXT,
            input("suffix", "Suffix", FieldKind::Text, true, "", "")
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "text.pad",
        "Text",
        "Pad",
        "Pads to a minimum width.",
        "leftpad rightpad align width fill",
        [
            TEXT,
            input("width", "Width", FieldKind::Number, true, "8", ""),
            choice("side", "Side", "start", &["start", "end"], ""),
            tucked(input(
                "fill",
                "Fill character",
                FieldKind::Text,
                false,
                " ",
                ""
            )),
        ],
        ["value"]
    ),
    task!(
        "text.repeat",
        "Text",
        "Repeat",
        "Repeats the text n times.",
        "duplicate times multiply",
        [
            TEXT,
            input("times", "Times", FieldKind::Number, true, "2", "")
        ],
        ["value"]
    ),
    task!(
        "text.indent",
        "Text",
        "Indent",
        "Prefixes every line with spaces.",
        "whitespace format block",
        [
            TEXT,
            input("spaces", "Spaces", FieldKind::Number, false, "2", "")
        ],
        ["value"]
    ),
    task!(
        "text.dedent",
        "Text",
        "Dedent",
        "Removes the common leading whitespace from every line.",
        "outdent unindent strip",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.squeeze",
        "Text",
        "Collapse whitespace",
        "Turns every run of whitespace into a single space and trims.",
        "normalize whitespace collapse clean single",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.lines",
        "Text",
        "Count lines",
        "Line count, and the first and last line.",
        "rows newline count",
        [TEXT],
        ["value", "first", "last"],
        DataShape::Number
    ),
    task!(
        "text.words",
        "Text",
        "Count words",
        "Whitespace-separated word count.",
        "wc tokens count",
        [TEXT],
        ["value"],
        DataShape::Number
    ),
    task!(
        "text.strip_ansi",
        "Text",
        "Strip ANSI colour",
        "Removes terminal escape sequences — what you want before putting captured output in a report.",
        "escape terminal colour color vt100 clean",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.snake",
        "Text",
        "snake_case",
        "Converts to snake_case.",
        "case identifier underscore",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.kebab",
        "Text",
        "kebab-case",
        "Converts to kebab-case.",
        "case identifier dash slug",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.camel",
        "Text",
        "camelCase",
        "Converts to camelCase.",
        "case identifier",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.pascal",
        "Text",
        "PascalCase",
        "Converts to PascalCase.",
        "case identifier upper",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.slug",
        "Text",
        "Slug",
        "A url-safe, lowercase, dash-separated form.",
        "url slugify permalink identifier",
        [TEXT],
        ["value"]
    ),
    task!(
        "text.between",
        "Text",
        "Between",
        "The text between the first `after` and the next `before`.",
        "extract substring delimiters parse scrape",
        [
            TEXT,
            input(
                "after",
                "After",
                FieldKind::Text,
                true,
                "",
                "The opening delimiter."
            ),
            input(
                "before",
                "Before",
                FieldKind::Text,
                true,
                "",
                "The closing delimiter."
            ),
        ],
        ["value", "found"]
    ),
    task!(
        "text.line_at",
        "Text",
        "Line at",
        "One line by 0-based index. Negative counts from the end.",
        "row nth index pick extract",
        [
            TEXT,
            input("index", "Index", FieldKind::Number, false, "0", "")
        ],
        ["value"]
    ),
    task!(
        "text.matching_lines",
        "Text",
        "Lines containing",
        "Keeps only the lines containing a literal substring — grep, without a node.",
        "grep filter search select rows",
        [
            TEXT,
            input("find", "Contains", FieldKind::Text, true, "", ""),
            choice(
                "mode",
                "Mode",
                "keep",
                &["keep", "drop"],
                "Keep matching lines, or drop them."
            ),
        ],
        ["value", "count"],
        DataShape::List
    ),
    // -- Number -------------------------------------------------------------
    task!(
        "number.add",
        "Number",
        "Add",
        "a + b.",
        "plus sum arithmetic math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.subtract",
        "Number",
        "Subtract",
        "a − b.",
        "minus difference math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.multiply",
        "Number",
        "Multiply",
        "a × b.",
        "times product math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.divide",
        "Number",
        "Divide",
        "a ÷ b. Fails on divide-by-zero rather than emitting `inf`.",
        "quotient ratio math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "1", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.modulo",
        "Number",
        "Modulo",
        "The remainder of a ÷ b.",
        "remainder mod math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "1", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.power",
        "Number",
        "Power",
        "a to the power of b.",
        "exponent pow math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "Exponent", FieldKind::Number, true, "2", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.abs",
        "Number",
        "Absolute",
        "Drops the sign.",
        "magnitude positive math",
        [input("value", "Value", FieldKind::Number, true, "0", "")],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.round",
        "Number",
        "Round",
        "Rounds to a number of decimal places.",
        "decimal precision math",
        [
            input("value", "Value", FieldKind::Number, true, "0", ""),
            input(
                "places",
                "Decimal places",
                FieldKind::Number,
                false,
                "0",
                ""
            )
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.floor",
        "Number",
        "Floor",
        "Rounds down.",
        "down truncate math",
        [input("value", "Value", FieldKind::Number, true, "0", "")],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.ceil",
        "Number",
        "Ceiling",
        "Rounds up.",
        "up math",
        [input("value", "Value", FieldKind::Number, true, "0", "")],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.min",
        "Number",
        "Minimum",
        "The smaller of two numbers.",
        "least lower smallest math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.max",
        "Number",
        "Maximum",
        "The larger of two numbers.",
        "greatest upper largest math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.clamp",
        "Number",
        "Clamp",
        "Constrains a number to a range.",
        "bound limit range math",
        [
            input("value", "Value", FieldKind::Number, true, "0", ""),
            input("min", "Minimum", FieldKind::Number, true, "0", ""),
            input("max", "Maximum", FieldKind::Number, true, "100", ""),
        ],
        ["value", "clamped"],
        DataShape::Number
    ),
    task!(
        "number.percent",
        "Number",
        "Percent of",
        "part ÷ whole, as a percentage.",
        "ratio proportion percentage math",
        [
            input("part", "Part", FieldKind::Number, true, "0", ""),
            input("whole", "Whole", FieldKind::Number, true, "100", ""),
            input(
                "places",
                "Decimal places",
                FieldKind::Number,
                false,
                "1",
                ""
            )
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "number.compare",
        "Number",
        "Compare",
        "`lt`, `eq` or `gt`, plus the three as booleans — everything a `when:` guard needs from one step.",
        "greater less equal cmp sort math",
        [
            input("a", "A", FieldKind::Number, true, "0", ""),
            input("b", "B", FieldKind::Number, true, "0", "")
        ],
        ["value", "lt", "eq", "gt"]
    ),
    task!(
        "number.format",
        "Number",
        "Format",
        "Fixed decimals, with optional thousands separators.",
        "decimal thousands commas pretty human math",
        [
            input("value", "Value", FieldKind::Number, true, "0", ""),
            input(
                "places",
                "Decimal places",
                FieldKind::Number,
                false,
                "2",
                ""
            ),
            tucked(input(
                "group",
                "Thousands separator",
                FieldKind::Text,
                false,
                "",
                "Leave empty for none."
            )),
        ],
        ["value"]
    ),
    task!(
        "number.bytes",
        "Number",
        "Humanize bytes",
        "1536 → 1.5 KB.",
        "size human readable filesize kb mb gb",
        [input("value", "Bytes", FieldKind::Number, true, "0", "")],
        ["value"]
    ),
    task!(
        "number.parse",
        "Number",
        "Parse number",
        "Pulls the first number out of arbitrary text. Fails if there isn't one.",
        "extract scan atoi coerce cast",
        [TEXT],
        ["value", "integer"],
        DataShape::Number
    ),
    // -- Logic --------------------------------------------------------------
    task!(
        "logic.check",
        "Logic",
        "Check",
        "Compares a value against something and answers true or false. The one node between \"a step produced this\" and \"so do that\".",
        "if condition compare match test equals contains greater branch guard when",
        [
            // Deliberately *not* required: `is empty` is one of the
            // operators, and a task whose whole job is inspecting whatever
            // turned up cannot refuse to run when nothing did. The
            // `explain` output is what makes an accidentally-empty value
            // visible instead of silently false.
            shaped(
                "value",
                "Value",
                FieldKind::Text,
                false,
                "",
                DataShape::Text,
                "Usually piped from an earlier step — press the link button."
            ),
            choice(
                "operator",
                "Is",
                "equals",
                &[
                    "equals",
                    "does not equal",
                    "contains",
                    "does not contain",
                    "starts with",
                    "ends with",
                    "matches",
                    "is empty",
                    "is not empty",
                    "is one of",
                    "greater than",
                    "at least",
                    "less than",
                    "at most"
                ],
                "`matches` takes a glob: `admin-*`, `web-?`. `is one of` takes a list."
            ),
            input(
                "compare_to",
                "Compared to",
                FieldKind::Text,
                false,
                "",
                "Ignored by `is empty` and `is not empty`."
            ),
            tucked(input(
                "ignore_case",
                "Ignore case",
                FieldKind::Toggle,
                false,
                "false",
                ""
            )),
            tucked(input(
                "trim",
                "Trim whitespace first",
                FieldKind::Toggle,
                false,
                "true",
                "On by default, because `whoami` prints a trailing newline and almost nobody wants to compare against one."
            )),
        ],
        ["value", "explain"],
        DataShape::Boolean
    ),
    task!(
        "logic.if",
        "Logic",
        "If",
        "Chooses between two values on a condition. The workhorse of a branch that does not need a whole subgraph.",
        "ternary branch condition choose select switch",
        [
            shaped(
                "condition",
                "Condition",
                FieldKind::Text,
                true,
                "",
                DataShape::Boolean,
                "Truthy: true, yes, 1, on, or any non-empty string other than false/no/0/off."
            ),
            input("then", "Then", FieldKind::Text, false, "", ""),
            input("else", "Else", FieldKind::Text, false, "", ""),
        ],
        ["value", "taken"]
    ),
    task!(
        "logic.equals",
        "Logic",
        "Equals",
        "Literal string equality.",
        "same compare eq match",
        [
            input("a", "A", FieldKind::Text, false, "", ""),
            input("b", "B", FieldKind::Text, false, "", ""),
            tucked(input(
                "ignore_case",
                "Ignore case",
                FieldKind::Toggle,
                false,
                "false",
                ""
            )),
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "logic.not",
        "Logic",
        "Not",
        "Inverts a boolean.",
        "negate invert opposite",
        [input("value", "Value", FieldKind::Text, true, "false", "")],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "logic.and",
        "Logic",
        "And",
        "True when both are truthy.",
        "both conjunction boolean",
        [
            input("a", "A", FieldKind::Text, true, "false", ""),
            input("b", "B", FieldKind::Text, true, "false", "")
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "logic.or",
        "Logic",
        "Or",
        "True when either is truthy.",
        "either disjunction boolean",
        [
            input("a", "A", FieldKind::Text, true, "false", ""),
            input("b", "B", FieldKind::Text, true, "false", "")
        ],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "logic.is_empty",
        "Logic",
        "Is empty",
        "True when the value is empty or whitespace.",
        "blank null none check",
        [input("value", "Value", FieldKind::Text, false, "", "")],
        ["value"],
        DataShape::Boolean
    ),
    task!(
        "logic.switch",
        "Logic",
        "Switch",
        "Matches a value against up to four cases and returns the paired result.",
        "case match lookup map table route",
        [
            input("value", "Value", FieldKind::Text, true, "", ""),
            input("case_1", "When", FieldKind::Text, false, "", ""),
            input("then_1", "Then", FieldKind::Text, false, "", ""),
            tucked(input("case_2", "When", FieldKind::Text, false, "", "")),
            tucked(input("then_2", "Then", FieldKind::Text, false, "", "")),
            tucked(input("case_3", "When", FieldKind::Text, false, "", "")),
            tucked(input("then_3", "Then", FieldKind::Text, false, "", "")),
            tucked(input("case_4", "When", FieldKind::Text, false, "", "")),
            tucked(input("then_4", "Then", FieldKind::Text, false, "", "")),
            input("fallback", "Otherwise", FieldKind::Text, false, "", ""),
        ],
        ["value", "matched"]
    ),
    // -- List ---------------------------------------------------------------
    task!(
        "list.split",
        "List",
        "Split",
        "Splits text into a list on a separator. Lists travel as separated strings, so any task here accepts any other task's output.",
        "explode tokenize separate parse csv",
        [
            TEXT,
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                ",",
                "Use \\n for newlines."
            )),
            tucked(input(
                "trim",
                "Trim each item",
                FieldKind::Toggle,
                false,
                "true",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.join",
        "List",
        "Join",
        "Re-joins a list with a different separator.",
        "implode concat glue",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Current separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
            input("with", "Join with", FieldKind::Text, false, ", ", ""),
        ],
        ["value", "count"]
    ),
    task!(
        "list.count",
        "List",
        "Count",
        "How many items.",
        "length size total",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            ))
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "list.get",
        "List",
        "Get item",
        "One item by 0-based index. Negative counts from the end.",
        "index nth pick element at",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            input("index", "Index", FieldKind::Number, false, "0", ""),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "found"]
    ),
    task!(
        "list.sort",
        "List",
        "Sort",
        "Alphabetical or numeric, ascending or descending.",
        "order arrange rank",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
            choice(
                "mode",
                "Order by",
                "text",
                &["text", "number", "length"],
                ""
            ),
            input(
                "descending",
                "Descending",
                FieldKind::Toggle,
                false,
                "false",
                ""
            ),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.unique",
        "List",
        "Unique",
        "Removes duplicates, keeping first appearance.",
        "dedupe distinct set",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            ))
        ],
        ["value", "count", "removed"],
        DataShape::List
    ),
    task!(
        "list.reverse",
        "List",
        "Reverse",
        "Reverses the order.",
        "flip backwards invert",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            ))
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.filter",
        "List",
        "Filter",
        "Keeps (or drops) items containing a substring.",
        "grep select where search",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            input("contains", "Contains", FieldKind::Text, true, "", ""),
            choice("mode", "Mode", "keep", &["keep", "drop"], ""),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.slice",
        "List",
        "Slice",
        "A range of items.",
        "take skip range page limit",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            input("start", "Start", FieldKind::Number, false, "0", ""),
            input(
                "end",
                "End",
                FieldKind::Number,
                false,
                "",
                "Exclusive. Empty means to the end."
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.map",
        "List",
        "Map with template",
        "Rewrites every item through a template. `{}` is the item, `{i}` its index.",
        "transform each format wrap prefix suffix",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            input(
                "template",
                "Template",
                FieldKind::Text,
                true,
                "{}",
                "`{}` is the item, `{i}` the index."
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "list.sum",
        "List",
        "Sum",
        "Adds every item up. Non-numeric items fail the step.",
        "total add aggregate",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            ))
        ],
        ["value", "count", "average", "min", "max"],
        DataShape::Number
    ),
    task!(
        "list.contains",
        "List",
        "Contains item",
        "Exact-match membership test.",
        "includes has member in",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            input("item", "Item", FieldKind::Text, true, "", ""),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "index"],
        DataShape::Boolean
    ),
    // -- JSON ---------------------------------------------------------------
    task!(
        "json.get",
        "JSON",
        "Get field",
        "Reads a value out of JSON by dotted path — `user.name`, `items.0.id`. The one that replaces `| jq -r`.",
        "jq path query extract read field pluck",
        [
            shaped(
                "json",
                "JSON",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            input(
                "path",
                "Path",
                FieldKind::Text,
                true,
                "",
                "Dots for objects, numbers for array indices."
            ),
            input("fallback", "If missing", FieldKind::Text, false, "", ""),
        ],
        ["value", "found", "type"]
    ),
    task!(
        "json.query",
        "JSON",
        "Query",
        "Pulls every value matching a path out of JSON. Wildcards, array filters and recursive search — the task to reach for when you do not already know the shape.",
        "jq jsonpath search extract select filter wildcard recursive path find",
        [
            shaped(
                "json",
                "JSON",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            input(
                "path",
                "Path",
                FieldKind::Text,
                true,
                "",
                concat!(
                    "`user.name` · `items[0].id` · `items[*].name` (every element) · ",
                    "`items[-1]` (last) · `items[?role=admin].name` (filter) · ",
                    "`..id` (at any depth)"
                )
            ),
            tucked(input(
                "separator",
                "Join matches with",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count", "first", "json"],
        DataShape::List
    ),
    task!(
        "json.set",
        "JSON",
        "Set field",
        "Writes a value at a dotted path, creating objects on the way.",
        "put write update patch modify",
        [
            shaped(
                "json",
                "JSON",
                FieldKind::Multiline,
                false,
                "{}",
                DataShape::Json,
                ""
            ),
            input("path", "Path", FieldKind::Text, true, "", ""),
            input("value", "Value", FieldKind::Text, false, "", ""),
            choice(
                "as",
                "Write as",
                "auto",
                &["auto", "string", "number", "boolean", "json"],
                "`auto` guesses; `json` parses the value as JSON first."
            ),
        ],
        ["value"],
        DataShape::Json
    ),
    task!(
        "json.keys",
        "JSON",
        "Keys",
        "The keys of an object, one per line.",
        "fields names properties list",
        [
            shaped(
                "json",
                "JSON",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            input(
                "path",
                "Path",
                FieldKind::Text,
                false,
                "",
                "Empty means the root."
            )
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "json.length",
        "JSON",
        "Length",
        "Array length, object key count, or string length.",
        "size count items",
        [
            shaped(
                "json",
                "JSON",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            input("path", "Path", FieldKind::Text, false, "", "")
        ],
        ["value", "type"],
        DataShape::Number
    ),
    task!(
        "json.pretty",
        "JSON",
        "Pretty-print",
        "Re-indents JSON. Also validates it.",
        "format indent beautify",
        [shaped(
            "json",
            "JSON",
            FieldKind::Multiline,
            true,
            "",
            DataShape::Json,
            ""
        )],
        ["value"],
        DataShape::Json
    ),
    task!(
        "json.compact",
        "JSON",
        "Minify",
        "Strips all insignificant whitespace.",
        "minify compress oneline",
        [shaped(
            "json",
            "JSON",
            FieldKind::Multiline,
            true,
            "",
            DataShape::Json,
            ""
        )],
        ["value"],
        DataShape::Json
    ),
    task!(
        "json.validate",
        "JSON",
        "Validate",
        "Succeeds when the text parses. Reports where it broke when it doesn't.",
        "parse check lint verify",
        [shaped(
            "json",
            "JSON",
            FieldKind::Multiline,
            true,
            "",
            DataShape::Json,
            ""
        )],
        ["value", "type"],
        DataShape::Boolean
    ),
    task!(
        "json.merge",
        "JSON",
        "Merge objects",
        "A shallow merge; the second object wins on conflicts.",
        "combine assign extend patch",
        [
            shaped(
                "a",
                "Base",
                FieldKind::Multiline,
                true,
                "{}",
                DataShape::Json,
                ""
            ),
            shaped(
                "b",
                "Overlay",
                FieldKind::Multiline,
                true,
                "{}",
                DataShape::Json,
                ""
            )
        ],
        ["value"],
        DataShape::Json
    ),
    task!(
        "json.array",
        "JSON",
        "List to JSON array",
        "Turns a separated list into a JSON array.",
        "convert encode wrap",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
            choice(
                "as",
                "Items are",
                "string",
                &["string", "number", "auto"],
                ""
            ),
        ],
        ["value", "count"],
        DataShape::Json
    ),
    task!(
        "json.pluck",
        "JSON",
        "Pluck from array",
        "Reads one field from every object in an array, as a list.",
        "map select column extract field array",
        [
            shaped(
                "json",
                "JSON array",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            input(
                "field",
                "Field",
                FieldKind::Text,
                true,
                "",
                "Dotted paths work."
            ),
            tucked(input(
                "separator",
                "Join with",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "json.escape",
        "JSON",
        "Escape string",
        "Quotes and escapes text as a JSON string literal.",
        "quote encode stringify",
        [TEXT],
        ["value"],
        DataShape::Json
    ),
    // -- Data ---------------------------------------------------------------
    task!(
        "data.type",
        "Data",
        "What shape is this?",
        "Detects whether a value is JSON, a number, a boolean, a list or plain text. Use it when you are wiring up something whose output you have not seen yet.",
        "detect inspect kind sniff shape validate format",
        [input("value", "Value", FieldKind::Multiline, true, "", "")],
        [
            "value",
            "is_json",
            "is_number",
            "is_boolean",
            "is_list",
            "length"
        ]
    ),
    task!(
        "data.expect",
        "Data",
        "Expect a shape",
        "Passes the value through when it is the shape you say, and fails the step with a readable message when it is not. A guard rail between two steps that disagree about their data.",
        "assert validate check type require guard format contract",
        [
            input("value", "Value", FieldKind::Multiline, true, "", ""),
            choice(
                "shape",
                "Must be",
                "json",
                &[
                    "json",
                    "json object",
                    "json array",
                    "number",
                    "boolean",
                    "list",
                    "not empty"
                ],
                ""
            ),
        ],
        ["value", "type"]
    ),
    task!(
        "data.json_to_list",
        "Data",
        "JSON array to list",
        "Turns a JSON array of scalars into the newline list every `list.*` task expects.",
        "convert flatten array items unwrap",
        [
            shaped(
                "json",
                "JSON array",
                FieldKind::Multiline,
                true,
                "",
                DataShape::Json,
                ""
            ),
            tucked(input(
                "separator",
                "Join with",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::List
    ),
    task!(
        "data.list_to_json",
        "Data",
        "List to JSON array",
        "The other direction: a newline list becomes a JSON array, typed the way you say.",
        "convert encode wrap array items",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            choice(
                "as",
                "Items are",
                "string",
                &["string", "number", "auto"],
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            )),
        ],
        ["value", "count"],
        DataShape::Json
    ),
    // -- Encoding -----------------------------------------------------------
    task!(
        "encode.base64",
        "Encoding",
        "Base64 encode",
        "Standard alphabet, padded.",
        "b64 encode binary",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.base64_decode",
        "Encoding",
        "Base64 decode",
        "Accepts padded or unpadded, standard or url-safe.",
        "b64 decode",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.base64url",
        "Encoding",
        "Base64url encode",
        "URL-safe alphabet, unpadded.",
        "b64 jwt url safe encode",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.base32",
        "Encoding",
        "Base32 encode",
        "RFC 4648 base32.",
        "encode otp",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.base32_decode",
        "Encoding",
        "Base32 decode",
        "RFC 4648 base32.",
        "decode otp",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.hex",
        "Encoding",
        "Hex encode",
        "Lowercase hex.",
        "base16 encode bytes",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.hex_decode",
        "Encoding",
        "Hex decode",
        "Accepts either case.",
        "base16 decode bytes",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.url",
        "Encoding",
        "URL encode",
        "Percent-encodes everything outside the unreserved set.",
        "percent escape query uri",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.url_decode",
        "Encoding",
        "URL decode",
        "Decodes percent-escapes and `+`.",
        "percent unescape query uri",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.html",
        "Encoding",
        "HTML escape",
        "Escapes &, <, >, \" and '.",
        "entities xml sanitize",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.html_decode",
        "Encoding",
        "HTML unescape",
        "Decodes the five named entities and numeric ones.",
        "entities xml",
        [TEXT],
        ["value"]
    ),
    task!(
        "encode.shell_quote",
        "Encoding",
        "Shell quote",
        "Single-quotes a value so it survives a POSIX shell intact. Use it whenever a value from earlier lands inside a `command:`.",
        "escape bash sh safe inject argument",
        [TEXT],
        ["value"]
    ),
    // -- Hash ---------------------------------------------------------------
    task!(
        "hash.blake3",
        "Hash",
        "BLAKE3",
        "The digest meshware uses everywhere else — same function as content ids.",
        "checksum digest fingerprint sum",
        [
            TEXT,
            tucked(input(
                "length",
                "Hex characters",
                FieldKind::Number,
                false,
                "64",
                "Truncates the digest. 64 is the full 32 bytes."
            ))
        ],
        ["value", "short"]
    ),
    task!(
        "hash.compare",
        "Hash",
        "Compare digests",
        "Constant-time-ish comparison of two digests, case- and whitespace-insensitive.",
        "verify checksum match equal integrity",
        [
            input("a", "Expected", FieldKind::Text, true, "", ""),
            input("b", "Actual", FieldKind::Text, true, "", "")
        ],
        ["value"],
        DataShape::Boolean
    ),
    // -- Path ---------------------------------------------------------------
    task!(
        "path.basename",
        "Path",
        "File name",
        "The last component.",
        "filename leaf name",
        [input("path", "Path", FieldKind::Text, true, "", "")],
        ["value", "stem", "extension"]
    ),
    task!(
        "path.dirname",
        "Path",
        "Directory",
        "Everything but the last component.",
        "parent folder dir",
        [input("path", "Path", FieldKind::Text, true, "", "")],
        ["value"]
    ),
    task!(
        "path.extension",
        "Path",
        "Extension",
        "The extension, without the dot.",
        "suffix type filetype",
        [input("path", "Path", FieldKind::Text, true, "", "")],
        ["value"]
    ),
    task!(
        "path.join",
        "Path",
        "Join",
        "Joins up to four segments with single slashes.",
        "concat combine build",
        [
            input("a", "First", FieldKind::Text, true, "", ""),
            input("b", "Second", FieldKind::Text, false, "", ""),
            input("c", "Third", FieldKind::Text, false, "", ""),
            input("d", "Fourth", FieldKind::Text, false, "", ""),
        ],
        ["value"]
    ),
    task!(
        "path.with_extension",
        "Path",
        "Replace extension",
        "Swaps the extension.",
        "rename suffix change",
        [
            input("path", "Path", FieldKind::Text, true, "", ""),
            input(
                "extension",
                "New extension",
                FieldKind::Text,
                true,
                "",
                "With or without the dot."
            )
        ],
        ["value"]
    ),
    task!(
        "path.normalize",
        "Path",
        "Normalize",
        "Collapses `.`, `..` and duplicate slashes.",
        "clean resolve canonical",
        [input("path", "Path", FieldKind::Text, true, "", "")],
        ["value", "absolute"]
    ),
    // -- Time ---------------------------------------------------------------
    task!(
        "time.now",
        "Time",
        "Now",
        "The coordinating node's clock. Not deterministic — two runs give two answers.",
        "date timestamp clock today current",
        [],
        ["value", "unix", "unix_ms", "date", "time"]
    ),
    task!(
        "time.format_unix",
        "Time",
        "Format timestamp",
        "Renders a unix timestamp as ISO-8601 UTC.",
        "date iso convert render epoch",
        [input(
            "unix",
            "Unix seconds",
            FieldKind::Number,
            true,
            "0",
            ""
        )],
        ["value", "date", "time"]
    ),
    task!(
        "time.parse",
        "Time",
        "Parse ISO date",
        "Reads an ISO-8601 UTC timestamp into unix seconds.",
        "date convert epoch iso8601",
        [input(
            "text",
            "ISO-8601",
            FieldKind::Text,
            true,
            "",
            "e.g. 2026-08-21T14:00:00Z"
        )],
        ["value", "unix"],
        DataShape::Number
    ),
    task!(
        "time.add",
        "Time",
        "Shift timestamp",
        "Adds a duration to a unix timestamp. Negative durations subtract.",
        "offset date arithmetic",
        [
            input("unix", "Unix seconds", FieldKind::Number, true, "0", ""),
            input(
                "duration",
                "Duration",
                FieldKind::Text,
                true,
                "1h",
                "e.g. 90s, 15m, 2h, 7d"
            ),
        ],
        ["value", "iso"],
        DataShape::Number
    ),
    task!(
        "time.diff",
        "Time",
        "Time between",
        "The gap between two unix timestamps.",
        "elapsed duration subtract age",
        [
            input("from", "From", FieldKind::Number, true, "0", ""),
            input("to", "To", FieldKind::Number, true, "0", "")
        ],
        ["value", "human", "minutes", "hours", "days"],
        DataShape::Number
    ),
    task!(
        "time.humanize",
        "Time",
        "Humanize duration",
        "90061 → `1d 1h 1m 1s`.",
        "readable pretty elapsed format",
        [input(
            "seconds",
            "Seconds",
            FieldKind::Number,
            true,
            "0",
            ""
        )],
        ["value"]
    ),
    task!(
        "time.parse_duration",
        "Time",
        "Parse duration",
        "`90s`, `15m`, `2h30m`, `7d` → seconds.",
        "convert humantime timeout",
        [input("duration", "Duration", FieldKind::Text, true, "", "")],
        ["value"],
        DataShape::Number
    ),
    // -- Random -------------------------------------------------------------
    task!(
        "random.id",
        "Random",
        "Random id",
        "A fresh lowercase base32 id. Not deterministic — every run and every retry gives a new one.",
        "uuid nanoid token unique key",
        [input(
            "length",
            "Characters",
            FieldKind::Number,
            false,
            "12",
            ""
        )],
        ["value"]
    ),
    task!(
        "random.number",
        "Random",
        "Random number",
        "An integer in an inclusive range. Not deterministic.",
        "dice sample range int",
        [
            input("min", "Minimum", FieldKind::Number, false, "0", ""),
            input("max", "Maximum", FieldKind::Number, false, "100", "")
        ],
        ["value"],
        DataShape::Number
    ),
    task!(
        "random.pick",
        "Random",
        "Pick one",
        "One item chosen from a list at random. Not deterministic.",
        "sample choose shuffle any",
        [
            shaped(
                "list",
                "List",
                FieldKind::Multiline,
                true,
                "",
                DataShape::List,
                ""
            ),
            tucked(input(
                "separator",
                "Separator",
                FieldKind::Text,
                false,
                "\\n",
                ""
            ))
        ],
        ["value", "index"]
    ),
];

/// Look a task up by id.
pub fn find(id: &str) -> Option<&'static TaskDef> {
    TASKS.iter().find(|t| t.id == id)
}

/// Every task in one category, in catalogue order.
pub fn in_category(category: &str) -> impl Iterator<Item = &'static TaskDef> {
    TASKS.iter().filter(move |t| t.category == category)
}

// ---------------------------------------------------------------------------
// Argument helpers
//
// Every one of these reports which input of which task was wrong. A step
// that fails with "expected a number" and no further detail is a step
// somebody has to open the YAML to debug.
// ---------------------------------------------------------------------------

type Args<'a> = &'a BTreeMap<String, String>;
type Out = std::result::Result<Vec<(String, String)>, String>;

fn declared(task: &str, name: &str) -> Option<&'static TaskInput> {
    find(task).and_then(|t| t.inputs.iter().find(|i| i.name == name))
}

/// An input's value, falling back to its declared default.
fn arg(task: &str, args: Args<'_>, name: &str) -> String {
    match args.get(name) {
        Some(v) if !v.is_empty() => v.clone(),
        Some(v) => {
            // An explicitly-empty value is a real answer for optional
            // inputs (an empty separator, an empty replacement) and must
            // not silently become the default.
            match declared(task, name) {
                Some(i) if i.required => i.default.to_string(),
                _ => v.clone(),
            }
        }
        None => declared(task, name)
            .map(|i| i.default)
            .unwrap_or("")
            .to_string(),
    }
}

/// A required input. Empty is an error, because a required input that
/// resolved to nothing usually means an upstream template produced nothing
/// — and failing here names the step that is actually wrong.
fn need(task: &str, args: Args<'_>, name: &str) -> std::result::Result<String, String> {
    let value = arg(task, args, name);
    if value.is_empty() {
        return Err(format!("`{name}` is required and resolved to nothing"));
    }
    Ok(value)
}

fn number(task: &str, args: Args<'_>, name: &str) -> std::result::Result<f64, String> {
    let raw = arg(task, args, name);
    raw.trim()
        .parse::<f64>()
        .map_err(|_| format!("`{name}` must be a number, got {raw:?}"))
}

fn integer(task: &str, args: Args<'_>, name: &str) -> std::result::Result<i64, String> {
    let raw = arg(task, args, name);
    let trimmed = raw.trim();
    trimmed.parse::<i64>().or_else(|_| {
        trimmed
            .parse::<f64>()
            .map(|f| f as i64)
            .map_err(|_| format!("`{name}` must be a whole number, got {raw:?}"))
    })
}

/// An optional integer: empty means "not given".
fn maybe_integer(
    task: &str,
    args: Args<'_>,
    name: &str,
) -> std::result::Result<Option<i64>, String> {
    if arg(task, args, name).trim().is_empty() {
        return Ok(None);
    }
    integer(task, args, name).map(Some)
}

/// meshware's one definition of truthy, used by every guard in the system.
/// Deliberately generous on the true side and explicit on the false side,
/// so `{{steps.check.outputs.value}}` works whether the upstream step
/// printed `true`, `1`, or `yes`.
pub fn truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "false" | "no" | "0" | "off" | "null" | "none" | "nil"
    )
}

fn flag(task: &str, args: Args<'_>, name: &str) -> bool {
    truthy(&arg(task, args, name))
}

/// Separators are written `\n` / `\t` in YAML-ish fields, where a literal
/// newline would be awkward to type into a one-line input.
fn separator(task: &str, args: Args<'_>, name: &str) -> String {
    let raw = arg(task, args, name);
    raw.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
}

fn split_list(text: &str, sep: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if sep.is_empty() {
        return text.chars().map(|c| c.to_string()).collect();
    }
    text.split(sep).map(|s| s.to_string()).collect()
}

/// Resolve a possibly-negative index against a length. `None` when it falls
/// outside the collection either way.
fn resolve_index(index: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let at = if index < 0 { len + index } else { index };
    (at >= 0 && at < len).then_some(at as usize)
}

fn ok(pairs: Vec<(&str, String)>) -> Out {
    Ok(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn one(value: impl Into<String>) -> Out {
    ok(vec![("value", value.into())])
}

fn boolean(value: bool) -> Out {
    one(if value { "true" } else { "false" })
}

/// Trim trailing zeros so `2 + 2` reports `4` rather than `4.0000000001`.
/// Every number this module emits goes through here, so arithmetic reads
/// the way it would on paper.
fn fmt_num(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut s = format!("{value:.10}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Run one built-in task.
///
/// `inputs` arrive already template-resolved: by the time this is called,
/// `{{steps.build.outputs.sha}}` is a literal string. The error case is a
/// message that names the input, and it becomes the step's `detail` — which
/// is the text a person reads when the run view turns that node red.
pub fn run(task: &str, inputs: &[(String, String)]) -> Out {
    let Some(def) = find(task) else {
        return Err(format!(
            "unknown task {task:?} — the palette lists {} built-ins",
            TASKS.len()
        ));
    };
    let args: BTreeMap<String, String> = inputs.iter().cloned().collect();
    for input in def.inputs {
        if input.required && arg(task, &args, input.name).is_empty() {
            return Err(format!(
                "`{}` is required and resolved to nothing",
                input.name
            ));
        }
    }
    match def.category {
        "Flow" => flow(task, &args),
        "Text" => text(task, &args),
        "Number" => number_task(task, &args),
        "Logic" => logic(task, &args),
        "List" => list(task, &args),
        "JSON" => json(task, &args),
        "Data" => data(task, &args),
        "Encoding" => encoding(task, &args),
        "Hash" => hash(task, &args),
        "Path" => path(task, &args),
        "Time" => time(task, &args),
        "Random" => random(task, &args),
        other => Err(format!(
            "task {task:?} has no evaluator (category {other:?})"
        )),
    }
}

fn flow(task: &str, a: Args<'_>) -> Out {
    match task {
        "flow.set" => one(arg(task, a, "value")),
        "flow.note" => one(arg(task, a, "text")),
        "flow.fail" => Err(arg(task, a, "message")),
        "flow.assert" => {
            if truthy(&arg(task, a, "condition")) {
                one("true")
            } else {
                Err(arg(task, a, "message"))
            }
        }
        "flow.default" => {
            let value = arg(task, a, "value");
            one(if value.trim().is_empty() {
                arg(task, a, "fallback")
            } else {
                value
            })
        }
        "flow.pick" => {
            for name in ["a", "b", "c", "d"] {
                let value = arg(task, a, name);
                if !value.trim().is_empty() {
                    return one(value);
                }
            }
            one("")
        }
        "flow.merge" => {
            let sep = arg(task, a, "separator");
            let parts: Vec<String> = ["a", "b", "c", "d"]
                .iter()
                .map(|n| arg(task, a, n))
                .filter(|v| !v.is_empty())
                .collect();
            one(parts.join(&sep))
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn text(task: &str, a: Args<'_>) -> Out {
    let text = arg(task, a, "text");
    match task {
        "text.upper" => one(text.to_uppercase()),
        "text.lower" => one(text.to_lowercase()),
        "text.title" => one(text
            .split_inclusive(char::is_whitespace)
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect::<String>()),
        "text.trim" => one(text.trim()),
        "text.trim_start" => one(text.trim_start()),
        "text.trim_end" => one(text.trim_end()),
        "text.length" => ok(vec![
            ("value", text.chars().count().to_string()),
            ("bytes", text.len().to_string()),
        ]),
        "text.reverse" => one(text.chars().rev().collect::<String>()),
        "text.replace" => {
            let find = need(task, a, "find")?;
            let with = arg(task, a, "replace");
            let count = text.matches(&find).count();
            ok(vec![
                ("value", text.replace(&find, &with)),
                ("count", count.to_string()),
            ])
        }
        "text.slice" => {
            let chars: Vec<char> = text.chars().collect();
            let start = resolve_index(integer(task, a, "start")?, chars.len().max(1))
                .unwrap_or(0)
                .min(chars.len());
            let end = match maybe_integer(task, a, "end")? {
                Some(e) => {
                    let len = chars.len() as i64;
                    let at = if e < 0 { len + e } else { e };
                    at.clamp(0, len) as usize
                }
                None => chars.len(),
            };
            one(chars[start.min(end)..end].iter().collect::<String>())
        }
        "text.truncate" => {
            let limit = integer(task, a, "limit")?.max(0) as usize;
            let suffix = arg(task, a, "suffix");
            let chars: Vec<char> = text.chars().collect();
            if chars.len() <= limit {
                return ok(vec![("value", text), ("truncated", "false".into())]);
            }
            let keep = limit.saturating_sub(suffix.chars().count());
            let cut: String = chars[..keep.min(chars.len())].iter().collect();
            ok(vec![
                ("value", format!("{cut}{suffix}")),
                ("truncated", "true".into()),
            ])
        }
        "text.contains" => boolean(text.contains(&need(task, a, "find")?)),
        "text.starts_with" => boolean(text.starts_with(&need(task, a, "prefix")?)),
        "text.ends_with" => boolean(text.ends_with(&need(task, a, "suffix")?)),
        "text.pad" => {
            let width = integer(task, a, "width")?.max(0) as usize;
            let fill = arg(task, a, "fill");
            let fill = fill.chars().next().unwrap_or(' ');
            let len = text.chars().count();
            if len >= width {
                return one(text);
            }
            let pad: String = std::iter::repeat_n(fill, width - len).collect();
            one(if arg(task, a, "side") == "end" {
                format!("{text}{pad}")
            } else {
                format!("{pad}{text}")
            })
        }
        "text.repeat" => {
            let times = integer(task, a, "times")?;
            if !(0..=10_000).contains(&times) {
                return Err(format!("`times` must be between 0 and 10000, got {times}"));
            }
            one(text.repeat(times as usize))
        }
        "text.indent" => {
            let spaces = integer(task, a, "spaces")?.clamp(0, 64) as usize;
            let pad = " ".repeat(spaces);
            one(text
                .lines()
                .map(|l| {
                    if l.is_empty() {
                        l.to_string()
                    } else {
                        format!("{pad}{l}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "text.dedent" => {
            let common = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            one(text
                .lines()
                .map(|l| {
                    if l.len() >= common {
                        &l[common..]
                    } else {
                        l.trim_start()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "text.squeeze" => one(text.split_whitespace().collect::<Vec<_>>().join(" ")),
        "text.lines" => {
            let lines: Vec<&str> = text.lines().collect();
            ok(vec![
                ("value", lines.len().to_string()),
                ("first", lines.first().unwrap_or(&"").to_string()),
                ("last", lines.last().unwrap_or(&"").to_string()),
            ])
        }
        "text.words" => one(text.split_whitespace().count().to_string()),
        "text.strip_ansi" => one(strip_ansi(&text)),
        "text.snake" => one(recase(&text, Case::Snake)),
        "text.kebab" => one(recase(&text, Case::Kebab)),
        "text.camel" => one(recase(&text, Case::Camel)),
        "text.pascal" => one(recase(&text, Case::Pascal)),
        "text.slug" => one(recase(&text, Case::Slug)),
        "text.between" => {
            let after = need(task, a, "after")?;
            let before = need(task, a, "before")?;
            match text.find(&after).map(|i| i + after.len()) {
                Some(start) => match text[start..].find(&before) {
                    Some(end) => ok(vec![
                        ("value", text[start..start + end].to_string()),
                        ("found", "true".into()),
                    ]),
                    None => ok(vec![("value", String::new()), ("found", "false".into())]),
                },
                None => ok(vec![("value", String::new()), ("found", "false".into())]),
            }
        }
        "text.line_at" => {
            let lines: Vec<&str> = text.lines().collect();
            match resolve_index(integer(task, a, "index")?, lines.len()) {
                Some(at) => one(lines[at]),
                None => Err(format!(
                    "line {} is outside a {}-line value",
                    arg(task, a, "index"),
                    lines.len()
                )),
            }
        }
        "text.matching_lines" => {
            let find = need(task, a, "find")?;
            let keep = arg(task, a, "mode") != "drop";
            let kept: Vec<&str> = text.lines().filter(|l| l.contains(&find) == keep).collect();
            ok(vec![
                ("value", kept.join("\n")),
                ("count", kept.len().to_string()),
            ])
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn number_task(task: &str, a: Args<'_>) -> Out {
    match task {
        "number.add" => one(fmt_num(number(task, a, "a")? + number(task, a, "b")?)),
        "number.subtract" => one(fmt_num(number(task, a, "a")? - number(task, a, "b")?)),
        "number.multiply" => one(fmt_num(number(task, a, "a")? * number(task, a, "b")?)),
        "number.divide" => {
            let b = number(task, a, "b")?;
            if b == 0.0 {
                return Err("cannot divide by zero".into());
            }
            one(fmt_num(number(task, a, "a")? / b))
        }
        "number.modulo" => {
            let b = number(task, a, "b")?;
            if b == 0.0 {
                return Err("cannot take a remainder modulo zero".into());
            }
            one(fmt_num(number(task, a, "a")? % b))
        }
        "number.power" => one(fmt_num(number(task, a, "a")?.powf(number(task, a, "b")?))),
        "number.abs" => one(fmt_num(number(task, a, "value")?.abs())),
        "number.round" => {
            let places = integer(task, a, "places")?.clamp(0, 12) as i32;
            let scale = 10f64.powi(places);
            one(fmt_num((number(task, a, "value")? * scale).round() / scale))
        }
        "number.floor" => one(fmt_num(number(task, a, "value")?.floor())),
        "number.ceil" => one(fmt_num(number(task, a, "value")?.ceil())),
        "number.min" => one(fmt_num(number(task, a, "a")?.min(number(task, a, "b")?))),
        "number.max" => one(fmt_num(number(task, a, "a")?.max(number(task, a, "b")?))),
        "number.clamp" => {
            let value = number(task, a, "value")?;
            let lo = number(task, a, "min")?;
            let hi = number(task, a, "max")?;
            if lo > hi {
                return Err(format!(
                    "minimum {} is above maximum {}",
                    fmt_num(lo),
                    fmt_num(hi)
                ));
            }
            let out = value.clamp(lo, hi);
            ok(vec![
                ("value", fmt_num(out)),
                ("clamped", (out != value).to_string()),
            ])
        }
        "number.percent" => {
            let whole = number(task, a, "whole")?;
            if whole == 0.0 {
                return Err("cannot take a percentage of zero".into());
            }
            let places = integer(task, a, "places")?.clamp(0, 12) as i32;
            let scale = 10f64.powi(places);
            one(fmt_num(
                ((number(task, a, "part")? / whole * 100.0) * scale).round() / scale,
            ))
        }
        "number.compare" => {
            let (x, y) = (number(task, a, "a")?, number(task, a, "b")?);
            let verdict = if x < y {
                "lt"
            } else if x > y {
                "gt"
            } else {
                "eq"
            };
            ok(vec![
                ("value", verdict.into()),
                ("lt", (x < y).to_string()),
                ("eq", (x == y).to_string()),
                ("gt", (x > y).to_string()),
            ])
        }
        "number.format" => {
            let places = integer(task, a, "places")?.clamp(0, 12) as usize;
            let rendered = format!("{:.places$}", number(task, a, "value")?);
            let group = arg(task, a, "group");
            one(if group.is_empty() {
                rendered
            } else {
                group_thousands(&rendered, &group)
            })
        }
        "number.bytes" => one(human_bytes(number(task, a, "value")?)),
        "number.parse" => {
            let text = arg(task, a, "text");
            match scan_number(&text) {
                Some(v) => ok(vec![("value", fmt_num(v)), ("integer", fmt_num(v.trunc()))]),
                None => Err(format!(
                    "no number found in {:?}",
                    truncate_for_error(&text)
                )),
            }
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn logic(task: &str, a: Args<'_>) -> Out {
    match task {
        "logic.if" => {
            let taken = truthy(&arg(task, a, "condition"));
            ok(vec![
                ("value", arg(task, a, if taken { "then" } else { "else" })),
                ("taken", if taken { "then".into() } else { "else".into() }),
            ])
        }
        "logic.check" => {
            let trim = flag(task, a, "trim");
            let fold = flag(task, a, "ignore_case");
            let prepare = |raw: String| {
                let t = if trim { raw.trim().to_string() } else { raw };
                if fold { t.to_lowercase() } else { t }
            };
            let value = prepare(arg(task, a, "value"));
            let other = prepare(arg(task, a, "compare_to"));
            let op = arg(task, a, "operator");
            // Numeric operators are the only ones that can fail, and they
            // fail loudly: silently answering `false` because a value was
            // not a number is how a workflow takes the wrong branch for an
            // hour before anybody notices.
            let numeric = || -> std::result::Result<(f64, f64), String> {
                let parse = |s: &str, which: &str| {
                    s.parse::<f64>()
                        .map_err(|_| format!("`{which}` must be a number for `{op}`, got {s:?}"))
                };
                Ok((parse(&value, "value")?, parse(&other, "compare_to")?))
            };
            let verdict = match op.as_str() {
                "equals" => value == other,
                "does not equal" => value != other,
                "contains" => value.contains(&other),
                "does not contain" => !value.contains(&other),
                "starts with" => value.starts_with(&other),
                "ends with" => value.ends_with(&other),
                "matches" => glob_matches(&other, &value),
                "is empty" => value.is_empty(),
                "is not empty" => !value.is_empty(),
                "is one of" => other
                    .split(['\n', ','])
                    .map(|item| item.trim())
                    .any(|item| item == value),
                "greater than" => {
                    let (x, y) = numeric()?;
                    x > y
                }
                "at least" => {
                    let (x, y) = numeric()?;
                    x >= y
                }
                "less than" => {
                    let (x, y) = numeric()?;
                    x < y
                }
                "at most" => {
                    let (x, y) = numeric()?;
                    x <= y
                }
                other_op => return Err(format!("unknown comparison {other_op:?}")),
            };
            // The explanation is the point. A boolean on its own tells you
            // which branch ran; this tells you why, in the run record,
            // without anybody adding an echo step to find out.
            let explain = if matches!(op.as_str(), "is empty" | "is not empty") {
                format!("{:?} {op}", truncate_for_error(&value))
            } else {
                format!(
                    "{:?} {op} {:?} → {verdict}",
                    truncate_for_error(&value),
                    truncate_for_error(&other)
                )
            };
            ok(vec![("value", verdict.to_string()), ("explain", explain)])
        }
        "logic.equals" => {
            let (x, y) = (arg(task, a, "a"), arg(task, a, "b"));
            boolean(if flag(task, a, "ignore_case") {
                x.eq_ignore_ascii_case(&y)
            } else {
                x == y
            })
        }
        "logic.not" => boolean(!truthy(&arg(task, a, "value"))),
        "logic.and" => boolean(truthy(&arg(task, a, "a")) && truthy(&arg(task, a, "b"))),
        "logic.or" => boolean(truthy(&arg(task, a, "a")) || truthy(&arg(task, a, "b"))),
        "logic.is_empty" => boolean(arg(task, a, "value").trim().is_empty()),
        "logic.switch" => {
            let value = arg(task, a, "value");
            for n in 1..=4 {
                let case = arg(task, a, &format!("case_{n}"));
                if !case.is_empty() && case == value {
                    return ok(vec![
                        ("value", arg(task, a, &format!("then_{n}"))),
                        ("matched", case),
                    ]);
                }
            }
            ok(vec![
                ("value", arg(task, a, "fallback")),
                ("matched", String::new()),
            ])
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn list(task: &str, a: Args<'_>) -> Out {
    let sep = separator(task, a, "separator");
    match task {
        "list.split" => {
            let mut items = split_list(&arg(task, a, "text"), &sep);
            if flag(task, a, "trim") {
                items = items.iter().map(|s| s.trim().to_string()).collect();
            }
            ok(vec![
                ("value", items.join("\n")),
                ("count", items.len().to_string()),
            ])
        }
        "list.join" => {
            let items = split_list(&arg(task, a, "list"), &sep);
            let with = separator(task, a, "with");
            ok(vec![
                ("value", items.join(&with)),
                ("count", items.len().to_string()),
            ])
        }
        "list.count" => one(split_list(&arg(task, a, "list"), &sep).len().to_string()),
        "list.get" => {
            let items = split_list(&arg(task, a, "list"), &sep);
            match resolve_index(integer(task, a, "index")?, items.len()) {
                Some(at) => ok(vec![("value", items[at].clone()), ("found", "true".into())]),
                None => ok(vec![("value", String::new()), ("found", "false".into())]),
            }
        }
        "list.sort" => {
            let mut items = split_list(&arg(task, a, "list"), &sep);
            match arg(task, a, "mode").as_str() {
                "number" => {
                    for item in &items {
                        if item.trim().parse::<f64>().is_err() {
                            return Err(format!("{item:?} is not a number — sort by text instead"));
                        }
                    }
                    items.sort_by(|x, y| {
                        let (x, y) = (
                            x.trim().parse::<f64>().unwrap_or(0.0),
                            y.trim().parse::<f64>().unwrap_or(0.0),
                        );
                        // total_cmp, not partial_cmp: "NaN" parses as a
                        // number, and a NaN that compares Equal to
                        // everything is not a total order — the sort panics.
                        x.total_cmp(&y)
                    });
                }
                "length" => items.sort_by_key(|i| i.chars().count()),
                _ => items.sort(),
            }
            if flag(task, a, "descending") {
                items.reverse();
            }
            ok(vec![
                ("value", items.join(&sep)),
                ("count", items.len().to_string()),
            ])
        }
        "list.unique" => {
            let items = split_list(&arg(task, a, "list"), &sep);
            let before = items.len();
            let mut seen = std::collections::BTreeSet::new();
            let kept: Vec<String> = items
                .into_iter()
                .filter(|i| seen.insert(i.clone()))
                .collect();
            ok(vec![
                ("count", kept.len().to_string()),
                ("removed", (before - kept.len()).to_string()),
                ("value", kept.join(&sep)),
            ])
        }
        "list.reverse" => {
            let mut items = split_list(&arg(task, a, "list"), &sep);
            items.reverse();
            ok(vec![
                ("value", items.join(&sep)),
                ("count", items.len().to_string()),
            ])
        }
        "list.filter" => {
            let contains = need(task, a, "contains")?;
            let keep = arg(task, a, "mode") != "drop";
            let kept: Vec<String> = split_list(&arg(task, a, "list"), &sep)
                .into_iter()
                .filter(|i| i.contains(&contains) == keep)
                .collect();
            ok(vec![
                ("value", kept.join(&sep)),
                ("count", kept.len().to_string()),
            ])
        }
        "list.slice" => {
            let items = split_list(&arg(task, a, "list"), &sep);
            let len = items.len() as i64;
            let start = {
                let s = integer(task, a, "start")?;
                (if s < 0 { len + s } else { s }).clamp(0, len) as usize
            };
            let end = match maybe_integer(task, a, "end")? {
                Some(e) => (if e < 0 { len + e } else { e }).clamp(0, len) as usize,
                None => items.len(),
            };
            let kept = &items[start.min(end)..end];
            ok(vec![
                ("value", kept.join(&sep)),
                ("count", kept.len().to_string()),
            ])
        }
        "list.map" => {
            let template = need(task, a, "template")?;
            let items = split_list(&arg(task, a, "list"), &sep);
            let mapped: Vec<String> = items
                .iter()
                .enumerate()
                .map(|(i, item)| template.replace("{}", item).replace("{i}", &i.to_string()))
                .collect();
            ok(vec![
                ("value", mapped.join(&sep)),
                ("count", mapped.len().to_string()),
            ])
        }
        "list.sum" => {
            let items = split_list(&arg(task, a, "list"), &sep);
            let mut values = Vec::with_capacity(items.len());
            for item in &items {
                values.push(
                    item.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("{item:?} is not a number"))?,
                );
            }
            if values.is_empty() {
                return Err("the list is empty — there is nothing to add up".into());
            }
            let total: f64 = values.iter().sum();
            let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            ok(vec![
                ("value", fmt_num(total)),
                ("count", values.len().to_string()),
                ("average", fmt_num(total / values.len() as f64)),
                ("min", fmt_num(lo)),
                ("max", fmt_num(hi)),
            ])
        }
        "list.contains" => {
            let item = need(task, a, "item")?;
            let items = split_list(&arg(task, a, "list"), &sep);
            match items.iter().position(|i| *i == item) {
                Some(at) => ok(vec![("value", "true".into()), ("index", at.to_string())]),
                None => ok(vec![("value", "false".into()), ("index", "-1".into())]),
            }
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

// ---------------------------------------------------------------------------
// JSON
//
// A dotted path, not JSONPath and not jq. `user.name`, `items.0.id`. That
// covers what a workflow does with an API response, and it is a syntax
// nobody has to look up.
// ---------------------------------------------------------------------------

fn parse_json(text: &str, field: &str) -> std::result::Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| format!("`{field}` is not valid JSON: {e}"))
}

fn walk<'v>(root: &'v serde_json::Value, path: &str) -> Option<&'v serde_json::Value> {
    let mut cursor = root;
    if path.trim().is_empty() {
        return Some(cursor);
    }
    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        cursor = match cursor {
            serde_json::Value::Object(map) => map.get(part)?,
            serde_json::Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// A JSON value as a *string a shell would want*: a JSON string yields its
/// contents without quotes, everything else its compact encoding. This is
/// the `jq -r` behaviour, and it is the one people expect.
fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn json(task: &str, a: Args<'_>) -> Out {
    match task {
        "json.get" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            match walk(&root, &need(task, a, "path")?) {
                Some(found) => ok(vec![
                    ("value", json_scalar(found)),
                    ("found", "true".into()),
                    ("type", json_type(found).into()),
                ]),
                None => ok(vec![
                    ("value", arg(task, a, "fallback")),
                    ("found", "false".into()),
                    ("type", "missing".into()),
                ]),
            }
        }
        "json.query" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            let path = need(task, a, "path")?;
            let matches = query_path(&root, &path)?;
            let sep = separator(task, a, "separator");
            let scalars: Vec<String> = matches.iter().map(|v| json_scalar(v)).collect();
            ok(vec![
                ("value", scalars.join(&sep)),
                ("count", matches.len().to_string()),
                ("first", scalars.first().cloned().unwrap_or_default()),
                (
                    "json",
                    serde_json::Value::Array(matches.into_iter().cloned().collect()).to_string(),
                ),
            ])
        }
        "json.set" => {
            let mut root = parse_json(&arg(task, a, "json"), "json")?;
            let path = need(task, a, "path")?;
            let raw = arg(task, a, "value");
            let value = match arg(task, a, "as").as_str() {
                "string" => serde_json::Value::String(raw),
                "number" => match raw.trim().parse::<i64>() {
                    Ok(n) => serde_json::Value::Number(n.into()),
                    Err(_) => serde_json::Value::Number(
                        serde_json::Number::from_f64(
                            raw.trim()
                                .parse::<f64>()
                                .map_err(|_| format!("`value` must be a number, got {raw:?}"))?,
                        )
                        .ok_or_else(|| format!("{raw:?} is not a finite number"))?,
                    ),
                },
                "boolean" => serde_json::Value::Bool(truthy(&raw)),
                "json" => parse_json(&raw, "value")?,
                _ => guess_json(&raw),
            };
            set_path(&mut root, &path, value)?;
            one(root.to_string())
        }
        "json.keys" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            let at = walk(&root, &arg(task, a, "path"))
                .ok_or_else(|| format!("no value at {:?}", arg(task, a, "path")))?;
            let keys: Vec<String> = match at {
                serde_json::Value::Object(map) => map.keys().cloned().collect(),
                serde_json::Value::Array(items) => {
                    (0..items.len()).map(|i| i.to_string()).collect()
                }
                other => return Err(format!("a {} has no keys", json_type(other))),
            };
            ok(vec![
                ("value", keys.join("\n")),
                ("count", keys.len().to_string()),
            ])
        }
        "json.length" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            let at = walk(&root, &arg(task, a, "path"))
                .ok_or_else(|| format!("no value at {:?}", arg(task, a, "path")))?;
            let len = match at {
                serde_json::Value::Array(items) => items.len(),
                serde_json::Value::Object(map) => map.len(),
                serde_json::Value::String(s) => s.chars().count(),
                serde_json::Value::Null => 0,
                other => return Err(format!("a {} has no length", json_type(other))),
            };
            ok(vec![
                ("value", len.to_string()),
                ("type", json_type(at).into()),
            ])
        }
        "json.pretty" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            one(serde_json::to_string_pretty(&root).unwrap_or_else(|_| root.to_string()))
        }
        "json.compact" => one(parse_json(&arg(task, a, "json"), "json")?.to_string()),
        "json.validate" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            ok(vec![
                ("value", "true".into()),
                ("type", json_type(&root).into()),
            ])
        }
        "json.merge" => {
            let base = parse_json(&arg(task, a, "a"), "a")?;
            let overlay = parse_json(&arg(task, a, "b"), "b")?;
            let (serde_json::Value::Object(mut base), serde_json::Value::Object(overlay)) =
                (base, overlay)
            else {
                return Err("both values must be JSON objects".into());
            };
            for (k, v) in overlay {
                base.insert(k, v);
            }
            one(serde_json::Value::Object(base).to_string())
        }
        "json.array" => {
            let sep = separator(task, a, "separator");
            let mode = arg(task, a, "as");
            let items: Vec<serde_json::Value> = split_list(&arg(task, a, "list"), &sep)
                .into_iter()
                .map(|item| match mode.as_str() {
                    "number" => match item.trim().parse::<i64>() {
                        Ok(n) => serde_json::Value::Number(n.into()),
                        Err(_) => item
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                    },
                    "auto" => guess_json(&item),
                    _ => serde_json::Value::String(item),
                })
                .collect();
            ok(vec![
                ("count", items.len().to_string()),
                ("value", serde_json::Value::Array(items).to_string()),
            ])
        }
        "json.pluck" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            let serde_json::Value::Array(items) = root else {
                return Err("the value must be a JSON array".into());
            };
            let field = need(task, a, "field")?;
            let sep = separator(task, a, "separator");
            let picked: Vec<String> = items
                .iter()
                .map(|item| walk(item, &field).map(json_scalar).unwrap_or_default())
                .collect();
            ok(vec![
                ("count", picked.len().to_string()),
                ("value", picked.join(&sep)),
            ])
        }
        "json.escape" => one(serde_json::Value::String(arg(task, a, "text")).to_string()),
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

// ---------------------------------------------------------------------------
// The query path
//
// Not JSONPath, not jq — a fifth of each, chosen by what a workflow does to
// an API response. Five segment kinds and no expression language:
//
//   name        an object key
//   [0] [-1]    an array index, negative from the end
//   [*]  *      every element of an array, every value of an object
//   [?k=v]      keep objects in an array whose `k` equals the literal `v`
//   ..          descend: match the rest of the path at any depth
//
// A path yields a *set* of matches, which is what makes `items[*].name` a
// one-step operation rather than a loop somebody has to build out of nodes.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(i64),
    All,
    /// Keep array elements whose field equals this literal.
    Filter {
        field: String,
        value: String,
    },
    /// Everything from here down.
    Descend,
}

fn parse_path(path: &str) -> std::result::Result<Vec<Segment>, String> {
    let chars: Vec<char> = path.trim().trim_start_matches('$').chars().collect();
    let mut out: Vec<Segment> = Vec::new();
    let mut i = 0;
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut Vec<Segment>| {
        if token.is_empty() {
            return;
        }
        out.push(if token == "*" {
            Segment::All
        } else {
            Segment::Key(std::mem::take(token))
        });
        token.clear();
    };
    while i < chars.len() {
        match chars[i] {
            '.' => {
                flush(&mut token, &mut out);
                // `a..b` — an empty segment is the descend operator.
                if chars.get(i + 1) == Some(&'.') {
                    out.push(Segment::Descend);
                    i += 1;
                }
                i += 1;
            }
            '[' => {
                flush(&mut token, &mut out);
                let close = chars[i..]
                    .iter()
                    .position(|c| *c == ']')
                    .map(|p| i + p)
                    .ok_or_else(|| format!("{path:?}: a `[` is never closed"))?;
                let inner: String = chars[i + 1..close].iter().collect();
                let inner = inner.trim();
                out.push(if inner == "*" {
                    Segment::All
                } else if let Some(filter) = inner.strip_prefix('?') {
                    let filter = filter.trim_start_matches('@').trim_start_matches('.');
                    let (field, value) = filter
                        .split_once(['=', ':'])
                        .ok_or_else(|| format!("{path:?}: a filter looks like `[?field=value]`"))?;
                    Segment::Filter {
                        field: field.trim().trim_end_matches('=').to_string(),
                        value: value.trim().trim_matches(['"', '\'']).to_string(),
                    }
                } else {
                    match inner.parse::<i64>() {
                        Ok(n) => Segment::Index(n),
                        // `["a key"]` is how you name a key with a dot in it.
                        Err(_) => Segment::Key(inner.trim_matches(['"', '\'']).to_string()),
                    }
                });
                i = close + 1;
            }
            c => {
                token.push(c);
                i += 1;
            }
        }
    }
    flush(&mut token, &mut out);
    Ok(out)
}

/// Every value reachable from `value`, itself first.
fn descendants<'v>(value: &'v serde_json::Value, out: &mut Vec<&'v serde_json::Value>) {
    out.push(value);
    match value {
        serde_json::Value::Object(map) => {
            for child in map.values() {
                descendants(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                descendants(child, out);
            }
        }
        _ => {}
    }
}

fn query_path<'v>(
    root: &'v serde_json::Value,
    path: &str,
) -> std::result::Result<Vec<&'v serde_json::Value>, String> {
    let segments = parse_path(path)?;
    let mut cursor: Vec<&serde_json::Value> = vec![root];
    for segment in &segments {
        let mut next: Vec<&serde_json::Value> = Vec::new();
        for value in &cursor {
            match segment {
                Segment::Descend => descendants(value, &mut next),
                Segment::All => match value {
                    serde_json::Value::Array(items) => next.extend(items.iter()),
                    serde_json::Value::Object(map) => next.extend(map.values()),
                    _ => {}
                },
                Segment::Key(name) => {
                    if let serde_json::Value::Object(map) = value
                        && let Some(found) = map.get(name)
                    {
                        next.push(found);
                    }
                }
                Segment::Index(at) => {
                    if let serde_json::Value::Array(items) = value
                        && let Some(found) =
                            resolve_index(*at, items.len()).and_then(|i| items.get(i))
                    {
                        next.push(found);
                    }
                }
                Segment::Filter { field, value: want } => {
                    if let serde_json::Value::Array(items) = value {
                        for item in items {
                            if walk(item, field).map(json_scalar).as_deref() == Some(want.as_str())
                            {
                                next.push(item);
                            }
                        }
                    }
                }
            }
        }
        // A descend followed by a key is the common `..id` form; without
        // dedup the same object matched through two parents appears twice.
        if matches!(segment, Segment::Descend) {
            let mut seen: Vec<*const serde_json::Value> = Vec::with_capacity(next.len());
            next.retain(|v| {
                let ptr = *v as *const serde_json::Value;
                if seen.contains(&ptr) {
                    false
                } else {
                    seen.push(ptr);
                    true
                }
            });
        }
        cursor = next;
    }
    Ok(cursor)
}

/// Shell-style glob: `*` any run, `?` one character. Not a regex, on
/// purpose — a regex in a workflow definition is a support ticket.
fn glob_matches(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            Some('?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

/// `auto` typing: what the value obviously is, defaulting to a string.
fn guess_json(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    match trimmed {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" => return serde_json::Value::Null,
        _ => {}
    }
    // Integers stay integral. `serde_json::Number::from_f64` would render
    // `1` as `1.0`, which is a different value to anything downstream that
    // compares JSON as text.
    if let Ok(n) = trimmed.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    if let Ok(n) = trimmed.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(n)
    {
        return serde_json::Value::Number(n);
    }
    serde_json::Value::String(raw.to_string())
}

fn set_path(
    root: &mut serde_json::Value,
    path: &str,
    value: serde_json::Value,
) -> std::result::Result<(), String> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    let Some((last, parents)) = parts.split_last() else {
        *root = value;
        return Ok(());
    };
    let mut cursor = root;
    for part in parents {
        // An array index addresses an existing slot; anything else is an
        // object key, and a missing one is created.
        if let Ok(index) = part.parse::<usize>() {
            let items = cursor.as_array_mut().ok_or_else(|| {
                format!("{part:?} is an array index but the value is not an array")
            })?;
            cursor = items
                .get_mut(index)
                .ok_or_else(|| format!("index {index} is past the end of the array"))?;
        } else {
            if !cursor.is_object() {
                *cursor = serde_json::Value::Object(serde_json::Map::new());
            }
            cursor = cursor
                .as_object_mut()
                .and_then(|m| {
                    m.entry(part.to_string())
                        .or_insert(serde_json::Value::Object(serde_json::Map::new()));
                    m.get_mut(*part)
                })
                .ok_or_else(|| format!("cannot descend into {part:?}"))?;
        }
    }
    if let Ok(index) = last.parse::<usize>() {
        let items = cursor
            .as_array_mut()
            .ok_or_else(|| format!("{last:?} is an array index but the value is not an array"))?;
        match items.get_mut(index) {
            Some(slot) => *slot = value,
            None => items.push(value),
        }
        return Ok(());
    }
    if !cursor.is_object() {
        *cursor = serde_json::Value::Object(serde_json::Map::new());
    }
    cursor
        .as_object_mut()
        .ok_or_else(|| format!("cannot set {last:?} on a non-object"))?
        .insert(last.to_string(), value);
    Ok(())
}

/// What a value looks like, by inspection. Order matters: `123` is a
/// number before it is text, and `[1,2]` is JSON before it is a one-line
/// list, because those are the readings anybody looking at the value would
/// give it.
fn detect_shape(raw: &str) -> (&'static str, usize) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ("empty", 0);
    }
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
        match parsed {
            serde_json::Value::Object(map) => return ("json object", map.len()),
            serde_json::Value::Array(items) => return ("json array", items.len()),
            serde_json::Value::Bool(_) => return ("boolean", 1),
            serde_json::Value::Number(_) => return ("number", 1),
            // A bare quoted string is JSON, but calling it that would be
            // pedantic — what the next step gets is text.
            _ => {}
        }
    }
    if trimmed.parse::<f64>().is_ok() {
        return ("number", 1);
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no"
    ) {
        return ("boolean", 1);
    }
    let lines = trimmed.lines().count();
    if lines > 1 {
        return ("list", lines);
    }
    ("text", trimmed.chars().count())
}

fn data(task: &str, a: Args<'_>) -> Out {
    match task {
        "data.type" => {
            let raw = arg(task, a, "value");
            let (shape, length) = detect_shape(&raw);
            ok(vec![
                ("value", shape.into()),
                ("is_json", shape.starts_with("json").to_string()),
                ("is_number", (shape == "number").to_string()),
                ("is_boolean", (shape == "boolean").to_string()),
                (
                    "is_list",
                    (shape == "list" || shape == "json array").to_string(),
                ),
                ("length", length.to_string()),
            ])
        }
        "data.expect" => {
            let raw = arg(task, a, "value");
            let (shape, _) = detect_shape(&raw);
            let want = arg(task, a, "shape");
            let held = match want.as_str() {
                "json" => shape.starts_with("json"),
                "json object" => shape == "json object",
                "json array" => shape == "json array",
                "number" => shape == "number",
                "boolean" => shape == "boolean",
                "list" => shape == "list" || shape == "json array",
                "not empty" => shape != "empty",
                other => return Err(format!("unknown shape {other:?}")),
            };
            if !held {
                return Err(format!(
                    "expected {want}, got {shape}: {:?}",
                    truncate_for_error(&raw)
                ));
            }
            ok(vec![("value", raw), ("type", shape.into())])
        }
        "data.json_to_list" => {
            let root = parse_json(&arg(task, a, "json"), "json")?;
            let serde_json::Value::Array(items) = root else {
                return Err("the value must be a JSON array".into());
            };
            let sep = separator(task, a, "separator");
            let scalars: Vec<String> = items.iter().map(json_scalar).collect();
            ok(vec![
                ("count", scalars.len().to_string()),
                ("value", scalars.join(&sep)),
            ])
        }
        "data.list_to_json" => {
            let sep = separator(task, a, "separator");
            let mode = arg(task, a, "as");
            let items: Vec<serde_json::Value> = split_list(&arg(task, a, "list"), &sep)
                .into_iter()
                .map(|item| match mode.as_str() {
                    "number" => match item.trim().parse::<i64>() {
                        Ok(n) => serde_json::Value::Number(n.into()),
                        Err(_) => item
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                    },
                    "auto" => guess_json(&item),
                    _ => serde_json::Value::String(item),
                })
                .collect();
            ok(vec![
                ("count", items.len().to_string()),
                ("value", serde_json::Value::Array(items).to_string()),
            ])
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn encoding(task: &str, a: Args<'_>) -> Out {
    use data_encoding::{BASE32, BASE64, BASE64URL_NOPAD, HEXLOWER_PERMISSIVE};
    let text = arg(task, a, "text");
    let decoded = |bytes: Vec<u8>| -> Out {
        match String::from_utf8(bytes) {
            Ok(s) => one(s),
            Err(_) => Err("the decoded bytes are not valid UTF-8".into()),
        }
    };
    match task {
        "encode.base64" => one(BASE64.encode(text.as_bytes())),
        "encode.base64url" => one(BASE64URL_NOPAD.encode(text.as_bytes())),
        "encode.base64_decode" => {
            // Accept every flavour anyone will paste in: padded or not,
            // standard or url-safe. Refusing a valid-looking token on a
            // padding technicality is not a useful error.
            let normalized: String = text
                .trim()
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(|c| match c {
                    '-' => '+',
                    '_' => '/',
                    other => other,
                })
                .collect();
            let padded = match normalized.len() % 4 {
                0 => normalized,
                n => format!("{normalized}{}", "=".repeat(4 - n)),
            };
            decoded(
                BASE64
                    .decode(padded.as_bytes())
                    .map_err(|e| format!("not valid base64: {e}"))?,
            )
        }
        "encode.base32" => one(BASE32.encode(text.as_bytes())),
        "encode.base32_decode" => decoded(
            BASE32
                .decode(text.trim().to_uppercase().as_bytes())
                .map_err(|e| format!("not valid base32: {e}"))?,
        ),
        "encode.hex" => one(HEXLOWER_PERMISSIVE.encode(text.as_bytes())),
        "encode.hex_decode" => decoded(
            HEXLOWER_PERMISSIVE
                .decode(text.trim().as_bytes())
                .map_err(|e| format!("not valid hex: {e}"))?,
        ),
        "encode.url" => {
            let mut out = String::with_capacity(text.len());
            for byte in text.bytes() {
                match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        out.push(byte as char)
                    }
                    other => out.push_str(&format!("%{other:02X}")),
                }
            }
            one(out)
        }
        "encode.url_decode" => {
            let bytes = text.as_bytes();
            let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'%' if i + 2 < bytes.len() => {
                        let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                        match u8::from_str_radix(hex, 16) {
                            Ok(byte) => {
                                out.push(byte);
                                i += 3;
                            }
                            Err(_) => return Err(format!("bad percent-escape at byte {i}")),
                        }
                    }
                    b'%' => return Err(format!("truncated percent-escape at byte {i}")),
                    b'+' => {
                        out.push(b' ');
                        i += 1;
                    }
                    other => {
                        out.push(other);
                        i += 1;
                    }
                }
            }
            decoded(out)
        }
        "encode.html" => one(text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")),
        "encode.html_decode" => one(unescape_html(&text)),
        "encode.shell_quote" => one(format!("'{}'", text.replace('\'', r#"'\''"#))),
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn hash(task: &str, a: Args<'_>) -> Out {
    match task {
        "hash.blake3" => {
            let digest = blake3::hash(arg(task, a, "text").as_bytes())
                .to_hex()
                .to_string();
            let length = integer(task, a, "length")?.clamp(4, 64) as usize;
            ok(vec![
                ("value", digest[..length].to_string()),
                ("short", digest[..12].to_string()),
            ])
        }
        "hash.compare" => {
            let norm = |s: String| s.trim().to_ascii_lowercase();
            boolean(norm(arg(task, a, "a")) == norm(arg(task, a, "b")))
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

fn path(task: &str, a: Args<'_>) -> Out {
    let raw = arg(task, a, "path");
    match task {
        "path.basename" => {
            let name = raw.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let (stem, extension) = match name.rsplit_once('.') {
                // A leading dot is part of the name, not an extension:
                // `.gitignore` has no extension.
                Some((stem, ext)) if !stem.is_empty() => (stem, ext),
                _ => (name, ""),
            };
            ok(vec![
                ("value", name.into()),
                ("stem", stem.into()),
                ("extension", extension.into()),
            ])
        }
        "path.dirname" => {
            let trimmed = raw.trim_end_matches('/');
            one(match trimmed.rfind('/') {
                Some(0) => "/".to_string(),
                Some(at) => trimmed[..at].to_string(),
                None => ".".to_string(),
            })
        }
        "path.extension" => {
            let name = raw.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            one(match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => ext.to_string(),
                _ => String::new(),
            })
        }
        "path.join" => {
            let mut out = String::new();
            for name in ["a", "b", "c", "d"] {
                let part = arg(task, a, name);
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if out.is_empty() {
                    out = part.to_string();
                } else {
                    out = format!(
                        "{}/{}",
                        out.trim_end_matches('/'),
                        part.trim_start_matches('/')
                    );
                }
            }
            one(out)
        }
        "path.with_extension" => {
            let ext = need(task, a, "extension")?;
            let ext = ext.trim_start_matches('.');
            let trimmed = raw.trim_end_matches('/');
            let cut = trimmed.rfind('/').map(|i| i + 1).unwrap_or(0);
            one(match trimmed[cut..].rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => {
                    format!("{}{stem}.{ext}", &trimmed[..cut])
                }
                _ => format!("{trimmed}.{ext}"),
            })
        }
        "path.normalize" => {
            let absolute = raw.starts_with('/');
            let mut parts: Vec<&str> = Vec::new();
            for part in raw.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        // `..` above the root of an absolute path is the
                        // root; on a relative path it has to be kept.
                        if matches!(parts.last(), Some(&last) if last != "..") {
                            parts.pop();
                        } else if !absolute {
                            parts.push("..");
                        }
                    }
                    other => parts.push(other),
                }
            }
            let joined = parts.join("/");
            let value = if absolute {
                format!("/{joined}")
            } else if joined.is_empty() {
                ".".to_string()
            } else {
                joined
            };
            ok(vec![("value", value), ("absolute", absolute.to_string())])
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

// ---------------------------------------------------------------------------
// Time
//
// A proleptic Gregorian calendar in forty lines, rather than a date crate.
// The workflow engine needs exactly two conversions — unix seconds to a UTC
// calendar date and back — and Howard Hinnant's civil-days algorithm is the
// whole of it. Adding a dependency (and its timezone database) to a signed,
// replicated execution engine for this would be a poor trade.
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 → (year, month, day). Valid across the whole
/// range a 64-bit unix timestamp can express.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Split unix seconds into an ISO-8601 date and time, both UTC.
fn iso_parts(unix: i64) -> (String, String) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        ),
    )
}

fn iso_stamp(unix: i64) -> String {
    let (date, time) = iso_parts(unix);
    format!("{date}T{time}Z")
}

/// `2026-08-21T14:00:00Z`, `2026-08-21 14:00:00`, `2026-08-21`. Offsets
/// other than `Z` are refused rather than silently read as UTC — a
/// timestamp that is four hours wrong is worse than one that failed.
fn parse_iso(text: &str) -> std::result::Result<i64, String> {
    let raw = text.trim();
    let body = raw
        .strip_suffix('Z')
        .or_else(|| raw.strip_suffix('z'))
        .unwrap_or(raw);
    // Past the date, which is ten *characters* and not ten bytes (S-L2).
    // `body[10..]` on a string whose eleventh byte lands mid-character is
    // an immediate panic, and this is reached from any timestamp filter a
    // person types. `char_indices` asks the question that was meant: is
    // there a sign after the date part?
    let after_date: String = body.chars().skip(10).collect();
    if after_date.contains('+') || after_date.chars().skip(1).any(|c| c == '-') {
        return Err(format!(
            "{raw:?} carries a UTC offset — give the time in UTC, ending in Z"
        ));
    }
    let (date, time) = match body.split_once(['T', ' ']) {
        Some((date, time)) => (date, time),
        None => (body, "00:00:00"),
    };
    let mut parts = date.split('-');
    let year: i64 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("{raw:?} does not start with a year"))?;
    let month: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
    let day: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("{raw:?} is not a real date"));
    }
    let mut clock = time.split(':');
    let hour: i64 = clock.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minute: i64 = clock.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    // Fractional seconds are accepted and truncated.
    let second: i64 = clock
        .next()
        .map(|p| p.split('.').next().unwrap_or("0"))
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// `90s`, `15m`, `2h30m`, `7d`, `-1h`, or a bare number of seconds.
fn parse_duration(text: &str) -> std::result::Result<i64, String> {
    let raw = text.trim();
    let (sign, body) = match raw.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, raw.strip_prefix('+').unwrap_or(raw)),
    };
    if body.is_empty() {
        return Err("a duration cannot be empty".into());
    }
    if let Ok(bare) = body.parse::<i64>() {
        return Ok(sign * bare);
    }
    let mut total: i64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;
    for ch in body.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let scale = match ch.to_ascii_lowercase() {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            _ => {
                return Err(format!(
                    "{text:?}: unknown unit {ch:?} — use s, m, h, d or w"
                ));
            }
        };
        let value: i64 = digits
            .parse()
            .map_err(|_| format!("{text:?}: {ch:?} has no number in front of it"))?;
        total += value * scale;
        digits.clear();
        saw_unit = true;
    }
    if !digits.is_empty() || !saw_unit {
        return Err(format!(
            "{text:?} is not a duration — try 90s, 15m, 2h30m or 7d"
        ));
    }
    Ok(sign * total)
}

fn humanize_secs(total: i64) -> String {
    if total == 0 {
        return "0s".into();
    }
    let sign = if total < 0 { "-" } else { "" };
    let mut left = total.abs();
    let mut parts: Vec<String> = Vec::new();
    for (unit, scale) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        let n = left / scale;
        if n > 0 {
            parts.push(format!("{n}{unit}"));
            left -= n * scale;
        }
    }
    format!("{sign}{}", parts.join(" "))
}

fn time(task: &str, a: Args<'_>) -> Out {
    match task {
        "time.now" => {
            let micros = mesh_proto::now_micros();
            let unix = (micros / 1_000_000) as i64;
            let (date, clock) = iso_parts(unix);
            ok(vec![
                ("value", iso_stamp(unix)),
                ("unix", unix.to_string()),
                ("unix_ms", (micros / 1_000).to_string()),
                ("date", date),
                ("time", clock),
            ])
        }
        "time.format_unix" => {
            let unix = integer(task, a, "unix")?;
            let (date, clock) = iso_parts(unix);
            ok(vec![
                ("value", iso_stamp(unix)),
                ("date", date),
                ("time", clock),
            ])
        }
        "time.parse" => {
            let unix = parse_iso(&need(task, a, "text")?)?;
            ok(vec![
                ("value", unix.to_string()),
                ("unix", unix.to_string()),
            ])
        }
        "time.add" => {
            let shifted = integer(task, a, "unix")? + parse_duration(&need(task, a, "duration")?)?;
            ok(vec![
                ("value", shifted.to_string()),
                ("iso", iso_stamp(shifted)),
            ])
        }
        "time.diff" => {
            let seconds = integer(task, a, "to")? - integer(task, a, "from")?;
            ok(vec![
                ("value", seconds.to_string()),
                ("human", humanize_secs(seconds)),
                ("minutes", fmt_num(seconds as f64 / 60.0)),
                ("hours", fmt_num(seconds as f64 / 3_600.0)),
                ("days", fmt_num(seconds as f64 / 86_400.0)),
            ])
        }
        "time.humanize" => one(humanize_secs(integer(task, a, "seconds")?)),
        "time.parse_duration" => one(parse_duration(&need(task, a, "duration")?)?.to_string()),
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

/// Uniform in `[0, bound)` by rejection sampling — a plain `%` would bias
/// the low end of the range, which shows up the moment anyone uses
/// `random.pick` on a short list.
fn random_below(bound: u64) -> u64 {
    if bound <= 1 {
        return 0;
    }
    let zone = u64::MAX - (u64::MAX % bound) - 1;
    loop {
        let mut buf = [0u8; 8];
        if getrandom::fill(&mut buf).is_err() {
            // Entropy is not optional on this platform, but a task must
            // still be total: fall back to the clock rather than panic.
            let micros = mesh_proto::now_micros();
            buf = micros.to_le_bytes();
        }
        let draw = u64::from_le_bytes(buf);
        if draw <= zone {
            return draw % bound;
        }
    }
}

fn random(task: &str, a: Args<'_>) -> Out {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyz23456789";
    match task {
        "random.id" => {
            let length = integer(task, a, "length")?.clamp(4, 64) as usize;
            let id: String = (0..length)
                .map(|_| ALPHABET[random_below(ALPHABET.len() as u64) as usize] as char)
                .collect();
            one(id)
        }
        "random.number" => {
            let lo = integer(task, a, "min")?;
            let hi = integer(task, a, "max")?;
            if lo > hi {
                return Err(format!("minimum {lo} is above maximum {hi}"));
            }
            let span = (hi - lo) as u64 + 1;
            one((lo + random_below(span) as i64).to_string())
        }
        "random.pick" => {
            let sep = separator(task, a, "separator");
            let items = split_list(&arg(task, a, "list"), &sep);
            if items.is_empty() {
                return Err("the list is empty — there is nothing to pick".into());
            }
            let at = random_below(items.len() as u64) as usize;
            ok(vec![
                ("value", items[at].clone()),
                ("index", at.to_string()),
            ])
        }
        _ => Err(format!("task {task:?} has no evaluator")),
    }
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Remove CSI / OSC escape sequences. Captured output from a build tool is
/// full of them, and they turn a workflow's summary into line noise.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ … final-byte in @–~
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … BEL or ESC \
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other two-byte escape.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Case {
    Snake,
    Kebab,
    Camel,
    Pascal,
    Slug,
}

/// Split on every boundary a human would recognise — separators, and the
/// lower→upper transition inside `parseHTTPResponse` — then rejoin. One
/// tokenizer for all five cases, so `text.snake` and `text.camel` can never
/// disagree about where a word starts.
fn words_of(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        let previous = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let starts_word = match previous {
            None => false,
            Some(prev) => {
                (ch.is_uppercase() && prev.is_lowercase())
                    // The last capital of a run belongs to the next word:
                    // HTTPResponse → http, response.
                    || (ch.is_uppercase()
                        && prev.is_uppercase()
                        && next.is_some_and(|n| n.is_lowercase()))
                // Digits do NOT start a word. `v2` is one token in
                // every slug anyone has ever wanted, and splitting it
                // would turn `release-v2` into `release-v-2`.
            }
        };
        if starts_word && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn recase(text: &str, case: Case) -> String {
    let words = words_of(text);
    match case {
        Case::Snake => words
            .iter()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join("_"),
        Case::Kebab | Case::Slug => words
            .iter()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join("-"),
        Case::Camel | Case::Pascal => words
            .iter()
            .enumerate()
            .map(|(i, w)| {
                if i == 0 && matches!(case, Case::Camel) {
                    return w.to_lowercase();
                }
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn unescape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            numeric if numeric.starts_with('#') => {
                let body = &numeric[1..];
                let code = match body.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => body.parse::<u32>().ok(),
                };
                code.and_then(char::from_u32)
            }
            _ => None,
        };
        match replacement {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn group_thousands(rendered: &str, group: &str) -> String {
    let (integer, fraction) = match rendered.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rendered, None),
    };
    let (sign, digits) = match integer.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", integer),
    };
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push_str(group);
        }
        grouped.push(ch);
    }
    match fraction {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

fn human_bytes(value: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let sign = if value < 0.0 { "-" } else { "" };
    let mut left = value.abs();
    let mut unit = 0;
    while left >= 1024.0 && unit < UNITS.len() - 1 {
        left /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{sign}{} B", left as i64);
    }
    format!("{sign}{left:.1} {}", UNITS[unit])
}

/// The first number in arbitrary text, sign and decimals included.
fn scan_number(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit()
            || (matches!(chars[i], '-' | '+' | '.')
                && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()));
        if !starts {
            i += 1;
            continue;
        }
        let start = i;
        if matches!(chars[i], '-' | '+') {
            i += 1;
        }
        let mut seen_dot = false;
        while i < chars.len() {
            match chars[i] {
                c if c.is_ascii_digit() => i += 1,
                '.' if !seen_dot && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) => {
                    seen_dot = true;
                    i += 1;
                }
                _ => break,
            }
        }
        if let Ok(value) = chars[start..i].iter().collect::<String>().parse::<f64>() {
            return Some(value);
        }
    }
    None
}

/// Error messages quote the offending value; a 4 MB JSON blob in a step's
/// `detail` helps nobody and lands in replicated state.
fn truncate_for_error(text: &str) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    format!("{}…", text.chars().take(MAX).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(task: &str, args: &[(&str, &str)]) -> BTreeMap<String, String> {
        let inputs: Vec<(String, String)> = args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        run(task, &inputs)
            .unwrap_or_else(|e| panic!("{task} failed: {e}"))
            .into_iter()
            .collect()
    }

    fn value(task: &str, args: &[(&str, &str)]) -> String {
        go(task, args).remove("value").unwrap_or_default()
    }

    /// Same thing, named so it does not read as a variable at call sites
    /// that already have a `value` of their own.
    fn value_of(task: &str, args: &[(&str, &str)]) -> String {
        value(task, args)
    }

    fn err(task: &str, args: &[(&str, &str)]) -> String {
        let inputs: Vec<(String, String)> = args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        run(task, &inputs).expect_err("expected a failure")
    }

    /// The catalogue is the API the Director renders. Anything listed and
    /// unimplemented would reach a user as a step that cannot run, so the
    /// table and the evaluators are checked against each other here rather
    /// than in review.
    #[test]
    fn every_catalogued_task_evaluates() {
        for def in TASKS {
            let inputs: Vec<(String, String)> = def
                .inputs
                .iter()
                .map(|i| {
                    // Something plausible for every field, so the arm is
                    // actually entered rather than refused at the required
                    // check.
                    let filler = match (i.kind, i.name) {
                        _ if !i.default.is_empty() => i.default.to_string(),
                        (FieldKind::Number, _) => "2".into(),
                        (FieldKind::Toggle, _) => "false".into(),
                        (_, "json" | "a" | "b") if def.category == "JSON" => "{\"k\":1}".into(),
                        (_, "path") => "a/b/c.txt".into(),
                        _ => "x".into(),
                    };
                    (i.name.to_string(), filler)
                })
                .collect();
            match run(def.id, &inputs) {
                // `flow.fail` failing is the point of `flow.fail`.
                Err(why) if def.id == "flow.fail" => assert_eq!(why, "failed on purpose"),
                Err(why) => assert!(
                    !why.contains("has no evaluator"),
                    "{} is in the catalogue but has no arm in run(): {why}",
                    def.id
                ),
                Ok(out) => {
                    let produced: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
                    for key in def.outputs {
                        assert!(
                            produced.contains(key),
                            "{} declares output {key:?} but did not emit it",
                            def.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn task_ids_are_unique_and_categorised() {
        let mut seen = std::collections::BTreeSet::new();
        for def in TASKS {
            assert!(seen.insert(def.id), "duplicate task id {}", def.id);
            assert!(
                CATEGORIES.iter().any(|(name, _)| *name == def.category),
                "{} is in unlisted category {:?}",
                def.id,
                def.category
            );
        }
    }

    #[test]
    fn an_unknown_task_names_itself() {
        let why = run("text.shout", &[]).expect_err("unknown task");
        assert!(why.contains("text.shout"), "{why}");
    }

    #[test]
    fn text_tasks_do_what_they_say() {
        assert_eq!(value("text.upper", &[("text", "hello")]), "HELLO");
        assert_eq!(
            value("text.slug", &[("text", "Rolling Upgrade v2!")]),
            "rolling-upgrade-v2"
        );
        assert_eq!(
            value("text.snake", &[("text", "parseHTTPResponse")]),
            "parse_http_response"
        );
        assert_eq!(
            value("text.camel", &[("text", "rolling-upgrade")]),
            "rollingUpgrade"
        );
        assert_eq!(
            value("text.pascal", &[("text", "rolling upgrade")]),
            "RollingUpgrade"
        );
        assert_eq!(value("text.squeeze", &[("text", "  a \n b   c ")]), "a b c");
        assert_eq!(
            value("text.slice", &[("text", "abcdef"), ("start", "-3")]),
            "def"
        );
        assert_eq!(
            value(
                "text.between",
                &[("text", "id=[42] rest"), ("after", "["), ("before", "]")]
            ),
            "42"
        );
        assert_eq!(
            value("text.strip_ansi", &[("text", "\u{1b}[31mred\u{1b}[0m")]),
            "red"
        );
        let truncated = go(
            "text.truncate",
            &[("text", "abcdefghij"), ("limit", "5"), ("suffix", "…")],
        );
        assert_eq!(truncated["value"], "abcd…");
        assert_eq!(truncated["truncated"], "true");
    }

    /// Character count and byte count differ the moment anyone types an
    /// accent, and a workflow that pads a column needs the first.
    #[test]
    fn length_reports_characters_and_bytes_separately() {
        let out = go("text.length", &[("text", "café")]);
        assert_eq!(out["value"], "4");
        assert_eq!(out["bytes"], "5");
    }

    #[test]
    fn arithmetic_reads_the_way_it_would_on_paper() {
        assert_eq!(value("number.add", &[("a", "2"), ("b", "2")]), "4");
        assert_eq!(value("number.divide", &[("a", "10"), ("b", "4")]), "2.5");
        assert_eq!(
            value("number.round", &[("value", "2.345"), ("places", "2")]),
            "2.35"
        );
        assert_eq!(value("number.bytes", &[("value", "1536")]), "1.5 KB");
        assert_eq!(
            value(
                "number.format",
                &[("value", "1234567.5"), ("places", "2"), ("group", ",")]
            ),
            "1,234,567.50"
        );
        assert_eq!(value("number.parse", &[("text", "took 42.5s")]), "42.5");
        assert!(err("number.divide", &[("a", "1"), ("b", "0")]).contains("divide by zero"));
    }

    #[test]
    fn comparison_gives_a_guard_everything_it_needs() {
        let out = go("number.compare", &[("a", "3"), ("b", "7")]);
        assert_eq!(out["value"], "lt");
        assert_eq!(out["lt"], "true");
        assert_eq!(out["gt"], "false");
        assert_eq!(out["eq"], "false");
    }

    /// Truthiness is one definition across the whole engine; a step that
    /// printed `yes` and a step that printed `1` must branch the same way.
    #[test]
    fn truthiness_is_generous_on_true_and_explicit_on_false() {
        for yes in ["true", "TRUE", "yes", "1", "on", "ok", "anything"] {
            assert!(truthy(yes), "{yes:?} should be truthy");
        }
        for no in ["", "  ", "false", "FALSE", "no", "0", "off", "null", "none"] {
            assert!(!truthy(no), "{no:?} should be falsy");
        }
    }

    #[test]
    fn if_reports_which_branch_it_took() {
        let out = go(
            "logic.if",
            &[("condition", "yes"), ("then", "go"), ("else", "stop")],
        );
        assert_eq!(out["value"], "go");
        assert_eq!(out["taken"], "then");
        let out = go(
            "logic.if",
            &[("condition", "0"), ("then", "go"), ("else", "stop")],
        );
        assert_eq!(out["value"], "stop");
        assert_eq!(out["taken"], "else");
    }

    /// The scenario this task exists for: a command printed a name, and the
    /// next step has to decide whether it is the one we wanted. One node,
    /// no template arithmetic, and a trailing newline does not break it.
    #[test]
    fn check_compares_a_command_output_against_a_string() {
        let out = go(
            "logic.check",
            &[
                ("value", "root\n"),
                ("operator", "equals"),
                ("compare_to", "root"),
            ],
        );
        assert_eq!(out["value"], "true");
        assert!(out["explain"].contains("root"), "{}", out["explain"]);

        let out = go(
            "logic.check",
            &[
                ("value", "deploy"),
                ("operator", "equals"),
                ("compare_to", "root"),
            ],
        );
        assert_eq!(out["value"], "false");
    }

    #[test]
    fn check_covers_the_operators_a_branch_actually_needs() {
        let case = |value: &str, op: &str, other: &str| {
            value_of(
                "logic.check",
                &[("value", value), ("operator", op), ("compare_to", other)],
            )
        };
        assert_eq!(case("web-3", "starts with", "web"), "true");
        assert_eq!(case("web-3", "ends with", "3"), "true");
        assert_eq!(case("web-3", "contains", "b-"), "true");
        assert_eq!(case("web-3", "does not contain", "db"), "true");
        assert_eq!(case("web-3", "matches", "web-?"), "true");
        assert_eq!(case("web-3", "matches", "db-*"), "false");
        assert_eq!(case("web-3", "is one of", "web-1\nweb-3\nweb-9"), "true");
        assert_eq!(case("", "is empty", ""), "true");
        assert_eq!(case("7", "greater than", "3"), "true");
        assert_eq!(case("7", "at most", "7"), "true");
        assert_eq!(case("3", "less than", "7"), "true");
    }

    /// Answering `false` because a value was not a number is how a workflow
    /// takes the wrong branch for an hour before anybody notices.
    #[test]
    fn a_numeric_comparison_on_non_numbers_fails_rather_than_answering_false() {
        let why = err(
            "logic.check",
            &[
                ("value", "many"),
                ("operator", "greater than"),
                ("compare_to", "3"),
            ],
        );
        assert!(why.contains("must be a number"), "{why}");
        assert!(why.contains("many"), "{why}");
    }

    #[test]
    fn check_can_ignore_case_and_can_be_told_not_to_trim() {
        assert_eq!(
            value_of(
                "logic.check",
                &[
                    ("value", "ROOT"),
                    ("operator", "equals"),
                    ("compare_to", "root"),
                    ("ignore_case", "true")
                ]
            ),
            "true"
        );
        assert_eq!(
            value_of(
                "logic.check",
                &[
                    ("value", "root\n"),
                    ("operator", "equals"),
                    ("compare_to", "root"),
                    ("trim", "false")
                ]
            ),
            "false"
        );
    }

    #[test]
    fn globs_are_globs_and_not_regexes() {
        assert!(glob_matches("web-*", "web-42"));
        assert!(glob_matches("*-prod", "eu-prod"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(glob_matches("*", ""));
        // A regex metacharacter is a literal here.
        assert!(!glob_matches("a.c", "abc"));
        assert!(glob_matches("a.c", "a.c"));
    }

    // ---------------------------------------------------------------------
    // json.query
    // ---------------------------------------------------------------------

    const API: &str = r#"{
      "team": {"name": "ops"},
      "users": [
        {"id": 1, "name": "mia",  "role": "admin"},
        {"id": 2, "name": "sam",  "role": "dev"},
        {"id": 3, "name": "remy", "role": "admin"}
      ]
    }"#;

    #[test]
    fn query_walks_objects_and_arrays() {
        assert_eq!(
            value_of("json.query", &[("json", API), ("path", "team.name")]),
            "ops"
        );
        assert_eq!(
            value_of("json.query", &[("json", API), ("path", "users[0].name")]),
            "mia"
        );
        assert_eq!(
            value_of("json.query", &[("json", API), ("path", "users[-1].name")]),
            "remy"
        );
    }

    /// The one that makes it worth having: every element, in one step,
    /// without anybody building a loop out of nodes.
    #[test]
    fn query_fans_out_over_a_wildcard() {
        let out = go("json.query", &[("json", API), ("path", "users[*].name")]);
        assert_eq!(out["value"], "mia\nsam\nremy");
        assert_eq!(out["count"], "3");
        assert_eq!(out["first"], "mia");
        assert_eq!(out["json"], r#"["mia","sam","remy"]"#);
    }

    #[test]
    fn query_filters_an_array_by_a_field() {
        let out = go(
            "json.query",
            &[("json", API), ("path", "users[?role=admin].name")],
        );
        assert_eq!(out["value"], "mia\nremy");
        assert_eq!(out["count"], "2");
    }

    /// `..id` is the move when you have not seen the shape yet.
    #[test]
    fn query_descends_to_any_depth_without_repeating_itself() {
        let out = go("json.query", &[("json", API), ("path", "..id")]);
        assert_eq!(out["value"], "1\n2\n3");
        let out = go("json.query", &[("json", API), ("path", "..name")]);
        assert_eq!(out["count"], "4", "the team's name counts too");
    }

    #[test]
    fn a_query_that_matches_nothing_is_empty_rather_than_an_error() {
        let out = go("json.query", &[("json", API), ("path", "users[*].email")]);
        assert_eq!(out["count"], "0");
        assert_eq!(out["value"], "");
        assert_eq!(out["json"], "[]");
    }

    #[test]
    fn a_malformed_path_says_what_the_form_is() {
        assert!(err("json.query", &[("json", API), ("path", "users[0")]).contains("never closed"));
        assert!(
            err("json.query", &[("json", API), ("path", "users[?role]")])
                .contains("[?field=value]")
        );
    }

    // ---------------------------------------------------------------------
    // Data shapes
    // ---------------------------------------------------------------------

    #[test]
    fn type_detection_reads_a_value_the_way_a_person_would() {
        let shape = |v: &str| value_of("data.type", &[("value", v)]);
        assert_eq!(shape(r#"{"a":1}"#), "json object");
        assert_eq!(shape("[1,2,3]"), "json array");
        assert_eq!(shape("42"), "number");
        assert_eq!(shape("3.5"), "number");
        assert_eq!(shape("true"), "boolean");
        assert_eq!(shape("yes"), "boolean");
        assert_eq!(shape("a\nb\nc"), "list");
        assert_eq!(shape("hello"), "text");
        assert_eq!(shape("   "), "empty");
    }

    #[test]
    fn expect_passes_the_value_through_or_says_what_it_got() {
        assert_eq!(
            value_of(
                "data.expect",
                &[("value", "[1,2]"), ("shape", "json array")]
            ),
            "[1,2]"
        );
        let why = err("data.expect", &[("value", "hello"), ("shape", "json")]);
        assert!(why.contains("expected json"), "{why}");
        assert!(why.contains("got text"), "{why}");
    }

    #[test]
    fn json_arrays_and_lists_convert_both_ways() {
        let out = go("data.json_to_list", &[("json", r#"["a","b","c"]"#)]);
        assert_eq!(out["value"], "a\nb\nc");
        assert_eq!(out["count"], "3");
        let back = go("data.list_to_json", &[("list", "a\nb\nc")]);
        assert_eq!(back["value"], r#"["a","b","c"]"#);
        let numbers = go("data.list_to_json", &[("list", "1\n2"), ("as", "number")]);
        assert_eq!(numbers["value"], "[1,2]");
    }

    /// The shape metadata is what the Director draws on a field and what
    /// warns about a mismatched pipe, so a task that claims to produce JSON
    /// had better produce JSON.
    #[test]
    fn declared_output_shapes_match_what_the_tasks_return() {
        for (id, sample) in [
            ("json.pretty", vec![("json", "{\"a\":1}")]),
            ("data.list_to_json", vec![("list", "a\nb")]),
            ("json.array", vec![("list", "a\nb")]),
        ] {
            let def = find(id).expect(id);
            assert_eq!(def.produces, DataShape::Json, "{id}");
            let out = go(id, &sample);
            assert!(
                serde_json::from_str::<serde_json::Value>(&out["value"]).is_ok(),
                "{id} claims json but returned {:?}",
                out["value"]
            );
        }
        for id in ["logic.check", "logic.equals", "text.contains"] {
            assert_eq!(find(id).expect(id).produces, DataShape::Boolean, "{id}");
        }
    }

    #[test]
    fn lists_round_trip_through_split_and_join() {
        let split = go("list.split", &[("text", "a, b, c"), ("separator", ",")]);
        assert_eq!(split["value"], "a\nb\nc");
        assert_eq!(split["count"], "3");
        let joined = go("list.join", &[("list", "a\nb\nc"), ("with", " | ")]);
        assert_eq!(joined["value"], "a | b | c");
    }

    #[test]
    fn list_maths_reports_the_whole_summary() {
        let out = go("list.sum", &[("list", "1\n2\n3\n4")]);
        assert_eq!(out["value"], "10");
        assert_eq!(out["average"], "2.5");
        assert_eq!(out["min"], "1");
        assert_eq!(out["max"], "4");
        assert!(err("list.sum", &[("list", "1\nx")]).contains("not a number"));
    }

    #[test]
    fn sorting_numerically_refuses_non_numbers_instead_of_guessing() {
        assert_eq!(
            value("list.sort", &[("list", "10\n9\n100"), ("mode", "number")]),
            "9\n10\n100"
        );
        assert_eq!(value("list.sort", &[("list", "10\n9\n100")]), "10\n100\n9");
        assert!(err("list.sort", &[("list", "1\nx"), ("mode", "number")]).contains("not a number"));
    }

    #[test]
    fn negative_indices_count_from_the_end() {
        let out = go("list.get", &[("list", "a\nb\nc"), ("index", "-1")]);
        assert_eq!(out["value"], "c");
        assert_eq!(out["found"], "true");
        let out = go("list.get", &[("list", "a\nb\nc"), ("index", "9")]);
        assert_eq!(out["found"], "false");
    }

    #[test]
    fn json_get_reads_dotted_paths_like_jq_dash_r() {
        let doc = r#"{"user":{"name":"mia"},"items":[{"id":7},{"id":9}]}"#;
        assert_eq!(
            value("json.get", &[("json", doc), ("path", "user.name")]),
            "mia"
        );
        assert_eq!(
            value("json.get", &[("json", doc), ("path", "items.1.id")]),
            "9"
        );
        let missing = go(
            "json.get",
            &[("json", doc), ("path", "user.email"), ("fallback", "-")],
        );
        assert_eq!(missing["value"], "-");
        assert_eq!(missing["found"], "false");
        assert!(err("json.get", &[("json", "{oops"), ("path", "a")]).contains("not valid JSON"));
    }

    #[test]
    fn json_set_creates_the_objects_on_the_way() {
        let out = value(
            "json.set",
            &[("json", "{}"), ("path", "a.b.c"), ("value", "1")],
        );
        assert_eq!(out, r#"{"a":{"b":{"c":1}}}"#);
        let quoted = value(
            "json.set",
            &[
                ("json", "{}"),
                ("path", "v"),
                ("value", "1"),
                ("as", "string"),
            ],
        );
        assert_eq!(quoted, r#"{"v":"1"}"#);
    }

    #[test]
    fn pluck_turns_an_array_of_objects_into_a_column() {
        let out = go(
            "json.pluck",
            &[("json", r#"[{"id":"a"},{"id":"b"}]"#), ("field", "id")],
        );
        assert_eq!(out["value"], "a\nb");
        assert_eq!(out["count"], "2");
    }

    #[test]
    fn encodings_round_trip() {
        let secret = "hello, world";
        let b64 = value("encode.base64", &[("text", secret)]);
        assert_eq!(value("encode.base64_decode", &[("text", &b64)]), secret);
        let hex = value("encode.hex", &[("text", secret)]);
        assert_eq!(value("encode.hex_decode", &[("text", &hex)]), secret);
        let url = value("encode.url", &[("text", "a b&c=d")]);
        assert_eq!(url, "a%20b%26c%3Dd");
        assert_eq!(value("encode.url_decode", &[("text", &url)]), "a b&c=d");
        let html = value("encode.html", &[("text", "<a href=\"x\">")]);
        assert_eq!(
            value("encode.html_decode", &[("text", &html)]),
            "<a href=\"x\">"
        );
    }

    /// A JWT segment is url-safe and unpadded; refusing it on a padding
    /// technicality would make the task useless for the thing people
    /// reach for it to do.
    #[test]
    fn base64_decode_accepts_url_safe_and_unpadded_input() {
        assert_eq!(
            value("encode.base64_decode", &[("text", "aGVsbG8")]),
            "hello"
        );
        assert_eq!(value("encode.base64_decode", &[("text", "8J-Ygg")]), "😂");
    }

    /// The one task standing between "a value from earlier" and a shell
    /// injection in the next step's `command:`.
    #[test]
    fn shell_quote_survives_an_embedded_quote() {
        assert_eq!(
            value("encode.shell_quote", &[("text", "it's fine; rm -rf /")]),
            r#"'it'\''s fine; rm -rf /'"#
        );
    }

    #[test]
    fn paths_split_the_way_a_shell_would() {
        let out = go("path.basename", &[("path", "/var/log/app.tar.gz")]);
        assert_eq!(out["value"], "app.tar.gz");
        assert_eq!(out["stem"], "app.tar");
        assert_eq!(out["extension"], "gz");
        assert_eq!(
            value("path.dirname", &[("path", "/var/log/app.log")]),
            "/var/log"
        );
        assert_eq!(
            value("path.join", &[("a", "/var/"), ("b", "/log"), ("c", "x")]),
            "/var/log/x"
        );
        assert_eq!(
            value(
                "path.with_extension",
                &[("path", "/tmp/a.txt"), ("extension", ".md")]
            ),
            "/tmp/a.md"
        );
        // A dotfile is a name, not an extension.
        assert_eq!(value("path.extension", &[("path", ".gitignore")]), "");
    }

    #[test]
    fn normalize_resolves_dot_dot_without_escaping_the_root() {
        let out = go("path.normalize", &[("path", "/a/b/../c//./d")]);
        assert_eq!(out["value"], "/a/c/d");
        assert_eq!(out["absolute"], "true");
        assert_eq!(value("path.normalize", &[("path", "/../..")]), "/");
        assert_eq!(value("path.normalize", &[("path", "../x")]), "../x");
    }

    /// The calendar is hand-rolled, so it is checked against dates whose
    /// answers are known: an epoch, a leap day, and a pre-epoch instant.
    #[test]
    fn the_calendar_agrees_with_reality() {
        assert_eq!(iso_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_stamp(1_771_675_200), "2026-02-21T12:00:00Z");
        assert_eq!(iso_stamp(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(iso_stamp(-86_400), "1969-12-31T00:00:00Z");
        for unix in [0, 1, 86_399, 1_000_000_000, 1_771_675_200, -1_000_000] {
            assert_eq!(parse_iso(&iso_stamp(unix)).expect("round trip"), unix);
        }
    }

    #[test]
    fn durations_parse_the_forms_people_write() {
        assert_eq!(parse_duration("90s").expect("secs"), 90);
        assert_eq!(parse_duration("2h30m").expect("compound"), 9_000);
        assert_eq!(parse_duration("7d").expect("days"), 604_800);
        assert_eq!(parse_duration("-1h").expect("negative"), -3_600);
        assert_eq!(parse_duration("45").expect("bare"), 45);
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("10x").is_err());
        assert_eq!(humanize_secs(90_061), "1d 1h 1m 1s");
        assert_eq!(humanize_secs(0), "0s");
    }

    #[test]
    fn a_utc_offset_is_refused_rather_than_read_as_utc() {
        assert!(
            parse_iso("2026-08-21T14:00:00+04:00")
                .expect_err("offset")
                .contains("offset")
        );
    }

    #[test]
    fn time_shifts_report_both_forms() {
        let out = go("time.add", &[("unix", "0"), ("duration", "1d")]);
        assert_eq!(out["value"], "86400");
        assert_eq!(out["iso"], "1970-01-02T00:00:00Z");
        let diff = go("time.diff", &[("from", "0"), ("to", "3600")]);
        assert_eq!(diff["human"], "1h");
        assert_eq!(diff["hours"], "1");
    }

    #[test]
    fn random_ids_are_the_length_asked_for_and_not_repeated() {
        let a = value("random.id", &[("length", "16")]);
        let b = value("random.id", &[("length", "16")]);
        assert_eq!(a.chars().count(), 16);
        assert_ne!(a, b);
    }

    #[test]
    fn random_pick_stays_inside_the_list() {
        for _ in 0..64 {
            let out = go("random.pick", &[("list", "a\nb\nc")]);
            assert!(["a", "b", "c"].contains(&out["value"].as_str()));
        }
    }

    #[test]
    fn assert_and_fail_carry_their_message_into_the_failure() {
        assert_eq!(
            err(
                "flow.assert",
                &[("condition", "false"), ("message", "no nodes matched")]
            ),
            "no nodes matched"
        );
        assert_eq!(value("flow.assert", &[("condition", "1")]), "true");
        assert_eq!(
            err("flow.fail", &[("message", "rolled back")]),
            "rolled back"
        );
    }

    /// A required input that resolved to nothing almost always means an
    /// upstream template produced nothing. Naming the input is what turns
    /// that into a five-second fix.
    #[test]
    fn a_missing_required_input_names_itself() {
        let why = err("text.replace", &[("text", "abc"), ("find", "")]);
        assert!(why.contains("find"), "{why}");
    }

    #[test]
    fn optional_inputs_fall_back_to_their_declared_default() {
        // `separator` is not given, so the declared "\n" applies.
        assert_eq!(value("list.count", &[("list", "a\nb")]), "2");
        // An explicitly-empty optional value is honoured, not overridden.
        assert_eq!(
            value(
                "text.replace",
                &[("text", "aaa"), ("find", "a"), ("replace", "")]
            ),
            ""
        );
    }
}
