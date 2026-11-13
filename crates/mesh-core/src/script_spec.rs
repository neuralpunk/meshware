//! What a script says about itself (R2 P1–P2).
//!
//! Registering a script used to cost four concepts and a hand-written JSON
//! Schema: push a bundle, create a service, bind it to the bundle, and write
//! a contract so the Run screen has fields to show. Skip the contract — and
//! nothing forces you not to — and the service registers fine, then offers
//! *"None declared — whatever you pass is forwarded as-is"* and a Run screen
//! with no inputs at all. The script still runs; there is simply nothing to
//! drive it with. That is the shape of the problem this module removes.
//!
//! Two sources, in this order:
//!
//!   1. **A declaration header**, which is the honest way to say the things
//!      a parser cannot work out — which argument a list fills in, how many
//!      cores an item needs, what the script reads and writes.
//!   2. **Inference from the script itself**, for the arguments it visibly
//!      parses. Never as good as a declaration, always better than nothing,
//!      and marked as inferred wherever it is shown.
//!
//! ## The header
//!
//! ```bash
//! #!/usr/bin/env bash
//! #meshware: name transcode-media
//! #meshware: about Re-encode one media file
//! #meshware: item path
//! #meshware: needs cores=2
//! #meshware: reads $path
//! #meshware: writes $outdir
//! #meshware: tools ffmpeg, ffprobe
//! #meshware: arg path!                              "The file to encode"
//! #meshware: arg codec : one of libx265|libx264 = libx265  "Video encoder"
//! #meshware: arg crf : int = 28                     "Lower is bigger"
//! #meshware: arg strip : bool = true                "Remove metadata"
//! ```
//!
//! It is a comment block, so a script carrying one is still an ordinary
//! script that runs anywhere. `#meshware:` is the marker; the rest of the
//! line is a directive and its argument. Unknown directives are collected
//! rather than rejected, because a script written for a later version should
//! not fail to register on an earlier one.
//!
//! `reads` and `writes` exist for R4, which derives a job's sandbox grants
//! from them instead of asking an operator to edit `meshnode.toml` on every
//! node. They are parsed and carried now so that a script written today
//! needs no change then.

use std::collections::BTreeSet;

/// The kind of value an argument takes. These are exactly the five the
/// Director argument editor can render (`lib/contract.ts: ArgKind`), because
/// a sixth would compile into a schema its row editor could not read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgType {
    Text,
    Integer,
    Number,
    Bool,
    /// `one of a|b|c`
    Choice(Vec<String>),
}

impl ArgType {
    fn json_type(&self) -> &'static str {
        match self {
            ArgType::Text | ArgType::Choice(_) => "string",
            ArgType::Integer => "integer",
            ArgType::Number => "number",
            ArgType::Bool => "boolean",
        }
    }

    /// Parse the `: TYPE` clause. `None` for anything unrecognised, so a
    /// typo becomes plain text rather than a refusal to register.
    fn parse(s: &str) -> Option<ArgType> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("one of") {
            let choices: Vec<String> = rest
                .split('|')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            return (!choices.is_empty()).then_some(ArgType::Choice(choices));
        }
        match s {
            "int" | "integer" => Some(ArgType::Integer),
            "num" | "number" | "float" => Some(ArgType::Number),
            "bool" | "boolean" | "yesno" => Some(ArgType::Bool),
            "text" | "string" | "str" => Some(ArgType::Text),
            _ => None,
        }
    }
}

/// One declared (or inferred) argument.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub name: String,
    pub ty: ArgType,
    pub default: Option<String>,
    pub description: String,
    pub required: bool,
    /// True when this came from reading the script rather than from a
    /// declaration. Shown, never hidden: a guess presented as a fact is how
    /// a person ends up debugging the wrong thing.
    pub inferred: bool,
    /// Bounds and shape, for the arguments where "it is a number" is not
    /// the whole truth. These exist because a definitions file has room for
    /// them and a one-line comment does not; they compile to the standard
    /// `minimum`, `maximum` and `pattern` keywords, so the Run screen
    /// validates against them and Director's row editor carries them
    /// through untouched.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub pattern: Option<String>,
}

impl Arg {
    /// A plain text argument by that name — the base every parser builds on.
    fn named(name: impl Into<String>) -> Arg {
        Arg {
            name: name.into(),
            ty: ArgType::Text,
            default: None,
            description: String::new(),
            required: false,
            inferred: false,
            min: None,
            max: None,
            pattern: None,
        }
    }
}

/// How a script is *run*, remembered from the last time somebody set it up.
///
/// Everything above this describes the script: what it is called, what
/// arguments it takes, what one item of work needs. None of that is the
/// half a person actually spends time on. Getting a script to run the way
/// it should run means choosing a shape (once, once per item, or all at
/// once as a gang), a target, a spread, a time limit, a priority, what one
/// item needs, whether output comes home or stays where it was made — a
/// dozen decisions, in four disclosures, every single time.
///
/// Those decisions are a property of the script, not of the afternoon. A
/// transcode wants two cores an item and its output left where it lands; a
/// training run wants eight machines together and packed. Nobody wants to
/// re-derive that on the second run, and until now the file that exists to
/// hold what a script knows about itself had nowhere to put it.
///
/// Every field is optional and *absent means "not remembered"*, never
/// "off": a definitions file written before this existed must leave the Run
/// screen exactly as it found it, and a half-filled `run` block must leave
/// the fields it does not mention alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunDefaults {
    /// `once` | `peritem` | `together` — which of the three shapes this is.
    pub mode: Option<String>,
    /// The target selector, as the Run screen's "where" resolves it:
    /// `any`, `name=…`, `tag=…`, or a raw expression.
    pub on: Option<String>,
    /// `one` (the scheduler picks) | `all` (every match) | `some` (best N).
    pub spread: Option<String>,
    /// How many, when `spread` is `some`.
    pub count: Option<u32>,
    /// Dispatch at most this many at a time.
    pub batch: Option<u32>,
    /// `3` or `10%` — give up once this many have failed.
    pub halt_on_fail: Option<String>,
    /// `low` | `normal` | `high`.
    pub priority: Option<String>,
    /// A duration the way the CLI takes one: `30s`, `5m`, `2h`.
    pub timeout: Option<String>,
    /// Which argument each item fills in, for a per-item run. Distinct from
    /// [`ScriptSpec::item`], which is what the *script* declared: this is
    /// what the person actually chose, and it may differ.
    pub item_input: Option<String>,
    /// Members in an all-or-nothing gang.
    pub gang: Option<u32>,
    /// `pack` (shortest path between members) or `spread`.
    pub gang_placement: Option<String>,
    /// Re-run the whole gang when a member dies.
    pub gang_restart: Option<bool>,
    pub gpus: Option<u32>,
    /// Minimum memory on *each* device: `24G`, `48000`.
    pub vram: Option<String>,
    /// A device model substring, as the Run screen's box takes it.
    pub device: Option<String>,
    pub gpu_share: Option<bool>,
    /// Items per chunk, for a per-item run.
    pub chunk: Option<u32>,
    /// What one item needs. These three are **carried, not stored**: they
    /// are the same question as [`ScriptSpec::cores`]/`mem`/`disk`, which
    /// a definitions file already answers under `needs`, so the writers
    /// below hoist them there and [`Self::to_json`] never emits them.
    /// Two homes for one number is how a form and a file start disagreeing.
    pub cores: Option<u32>,
    pub mem: Option<String>,
    pub disk: Option<String>,
    /// Leave each item's output on the node that produced it.
    pub keep_output: Option<bool>,
    pub reduce: Option<String>,
    pub reduce_on: Option<String>,
    /// Collect an answer from every target.
    pub capture: Option<bool>,
    /// An expectation asserted row by row on the origin.
    pub expect: Option<String>,
    /// Per-run environment. Sorted, so the file diffs line by line.
    pub env: Vec<(String, String)>,
    /// Inputs the contract does not declare, supplied anyway.
    pub inputs: Vec<(String, String)>,
}

impl RunDefaults {
    /// True when nothing here was remembered.
    pub fn is_empty(&self) -> bool {
        *self == RunDefaults::default()
    }

    /// Take everything `other` remembers, keeping what it is silent about.
    ///
    /// Field by field rather than wholesale, for the same reason
    /// [`ScriptSpec::merge_over`] is: a `run` block that sets only
    /// `priority` must not erase the timeout the header set.
    fn merge_over(&mut self, other: RunDefaults) {
        macro_rules! take {
            ($($f:ident),* $(,)?) => { $( if other.$f.is_some() { self.$f = other.$f; } )* };
        }
        take!(
            mode,
            on,
            spread,
            count,
            batch,
            halt_on_fail,
            priority,
            timeout,
            item_input,
            gang,
            gang_placement,
            gang_restart,
            gpus,
            vram,
            device,
            gpu_share,
            chunk,
            cores,
            mem,
            disk,
            keep_output,
            reduce,
            reduce_on,
            capture,
            expect,
        );
        if !other.env.is_empty() {
            self.env = other.env;
        }
        if !other.inputs.is_empty() {
            self.inputs = other.inputs;
        }
    }
}

/// Everything a script says about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScriptSpec {
    pub name: Option<String>,
    pub about: String,
    /// Which argument a per-item run fills in with each item.
    pub item: Option<String>,
    pub cores: Option<u32>,
    pub mem: Option<String>,
    pub disk: Option<String>,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    /// Commands this script needs on the machine that runs it.
    pub tools: Vec<String>,
    pub args: Vec<Arg>,
    /// How this script was last set up to run — see [`RunDefaults`]. Only
    /// a definitions file carries this; there is no header directive for
    /// it, because it is two dozen keys and a header is one line.
    pub run: RunDefaults,
    /// Directives this build did not recognise, kept so a newer script
    /// registers on an older node instead of failing.
    pub unknown: Vec<String>,
    /// The unrecognised top-level keys of a definitions file, with their
    /// values — as against [`Self::unknown`], which is only their names.
    ///
    /// `unknown` alone was enough while the only thing that ever rewrote
    /// one of these files was a person pressing Save in an editor they
    /// were looking at. Saving run defaults rewrites the file as a side
    /// effect of pressing Run, and a rewrite that silently drops a key
    /// somebody hand-wrote — because this build happens not to know it —
    /// is the kind of thing you discover a fortnight later.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

const MARKER: &str = "#meshware:";

/// The contract keyword carrying everything about a script that is not one
/// of its arguments. `x-` marks it as an extension; JSON Schema validators
/// ignore unknown keywords, which is what makes this safe to put here.
pub const EXTENSION: &str = "x-meshware";

impl ScriptSpec {
    /// Read a script: its header first, then inference for anything the
    /// header did not declare.
    pub fn read(source: &str) -> ScriptSpec {
        let mut spec = ScriptSpec::from_header(source);
        let declared: BTreeSet<String> = spec.args.iter().map(|a| a.name.clone()).collect();
        for arg in infer_bash_args(source) {
            if !declared.contains(&arg.name) {
                spec.args.push(arg);
            }
        }
        spec.args.sort_by(|a, b| a.name.cmp(&b.name));
        spec
    }

    /// Only the declaration header, with nothing inferred.
    pub fn from_header(source: &str) -> ScriptSpec {
        let mut spec = ScriptSpec::default();
        for line in source.lines() {
            let line = line.trim_start();
            let Some(rest) = line.strip_prefix(MARKER) else {
                continue;
            };
            let rest = rest.trim();
            let (directive, value) = match rest.split_once(char::is_whitespace) {
                Some((d, v)) => (d, v.trim()),
                None => (rest, ""),
            };
            match directive {
                "name" => spec.name = Some(value.to_string()),
                "about" | "description" => spec.about = value.to_string(),
                "item" => spec.item = Some(value.trim_start_matches("--").to_string()),
                "needs" => parse_needs(value, &mut spec),
                "reads" => spec.reads.push(value.to_string()),
                "tools" | "needs-tools" => spec.tools.extend(
                    value
                        .split([',', ' '])
                        .filter(|t| !t.is_empty())
                        .map(String::from),
                ),
                "writes" => spec.writes.push(value.to_string()),
                "arg" => {
                    if let Some(arg) = parse_arg(value) {
                        spec.args.push(arg);
                    }
                }
                _ => spec.unknown.push(rest.to_string()),
            }
        }
        spec
    }

    /// Whether the script said anything about itself at all.
    pub fn has_header(&self) -> bool {
        self.name.is_some()
            || !self.about.is_empty()
            || self.item.is_some()
            || !self.args.iter().all(|a| a.inferred)
    }

    /// The JSON Schema a service is registered with.
    ///
    /// Deliberately restricted to what Director's row editor round-trips —
    /// `type`, `description`, `default`, `enum`, `required` and
    /// `additionalProperties` — so a contract generated here can be edited
    /// in the GUI afterwards and still be the same shape.
    pub fn contract_json(&self) -> Option<String> {
        if self.args.is_empty() {
            return None;
        }
        let mut props = serde_json::Map::new();
        let mut required: Vec<serde_json::Value> = Vec::new();
        for arg in &self.args {
            let mut p = serde_json::Map::new();
            p.insert("type".into(), arg.ty.json_type().into());
            if !arg.description.is_empty() {
                p.insert("description".into(), arg.description.clone().into());
            }
            if let ArgType::Choice(choices) = &arg.ty {
                p.insert(
                    "enum".into(),
                    serde_json::Value::Array(choices.iter().map(|c| c.clone().into()).collect()),
                );
            }
            if let Some(d) = &arg.default {
                p.insert("default".into(), coerce(d, &arg.ty));
            }
            // Standard keywords, so they validate on the way in and survive
            // Director's row editor as unmodelled `extra` on the way out.
            if let Some(v) = arg.min {
                p.insert("minimum".into(), number(v, &arg.ty));
            }
            if let Some(v) = arg.max {
                p.insert("maximum".into(), number(v, &arg.ty));
            }
            if let Some(v) = &arg.pattern {
                p.insert("pattern".into(), v.clone().into());
            }
            props.insert(arg.name.clone(), serde_json::Value::Object(p));
            if arg.required {
                required.push(arg.name.clone().into());
            }
        }
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), "object".into());
        if !self.about.is_empty() {
            schema.insert("description".into(), self.about.clone().into());
        }
        schema.insert("properties".into(), serde_json::Value::Object(props));
        if !required.is_empty() {
            schema.insert("required".into(), serde_json::Value::Array(required));
        }
        schema.insert("additionalProperties".into(), false.into());
        if let Some(extras) = self.extension() {
            schema.insert(EXTENSION.into(), extras);
        }
        serde_json::to_string_pretty(&serde_json::Value::Object(schema)).ok()
    }

    /// The half of the header that is not an argument — which input a list
    /// fills in, what one item needs, what it reads and writes.
    ///
    /// It travels in the contract under [`EXTENSION`] rather than in a new
    /// field on `ServiceVersion`, because a JSON Schema may carry unknown
    /// keywords and a wire format may not. Every validator ignores it; the
    /// Run screen reads it to propose a plan, and R4 will read it to derive
    /// a job's sandbox grants.
    fn extension(&self) -> Option<serde_json::Value> {
        let mut m = serde_json::Map::new();
        if let Some(item) = &self.item {
            m.insert("item".into(), item.clone().into());
        }
        if let Some(c) = self.cores {
            m.insert("cores".into(), c.into());
        }
        if let Some(v) = &self.mem {
            m.insert("mem".into(), v.clone().into());
        }
        if let Some(v) = &self.disk {
            m.insert("disk".into(), v.clone().into());
        }
        if !self.reads.is_empty() {
            m.insert("reads".into(), self.reads.clone().into());
        }
        if !self.writes.is_empty() {
            m.insert("writes".into(), self.writes.clone().into());
        }
        if !self.tools.is_empty() {
            m.insert("tools".into(), self.tools.clone().into());
        }
        // The run defaults ride here too, and this is the half that makes
        // the feature work at all: a definitions file lives on one
        // machine's disk, and the contract is replicated. A person who
        // sets a script up on their laptop and then runs it from the
        // office box gets the same form pre-filled, because the *mesh*
        // knows how this script is run and not just the folder it was
        // pushed from.
        if let Some(run) = self.run.to_json() {
            m.insert("run".into(), run);
        }
        (!m.is_empty()).then_some(serde_json::Value::Object(m))
    }

    /// Read back what [`Self::extension`] wrote, from a registered
    /// service's contract. Absent or malformed is simply "not declared" —
    /// a service registered before this existed must keep working.
    pub fn from_contract(contract: &str) -> ScriptSpec {
        let mut spec = ScriptSpec::default();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(contract) else {
            return spec;
        };
        spec.about = v["description"].as_str().unwrap_or_default().to_string();
        let ext = &v[EXTENSION];
        spec.item = ext["item"].as_str().map(|s| s.to_string());
        spec.cores = ext["cores"].as_u64().map(|c| c as u32);
        spec.mem = ext["mem"].as_str().map(|s| s.to_string());
        spec.disk = ext["disk"].as_str().map(|s| s.to_string());
        let list = |k: &str| -> Vec<String> {
            ext[k]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        spec.reads = list("reads");
        spec.writes = list("writes");
        spec.tools = list("tools");
        spec.run = RunDefaults::from_json(&ext["run"]);
        spec
    }
}

impl RunDefaults {
    /// This block as JSON, or `None` when nothing was remembered.
    ///
    /// One canonical spelling on the way out, a forgiving reader on the way
    /// in — the same split the rest of this module uses. Keys are written
    /// in a fixed order (shape, then where, then how much, then what comes
    /// back) so the file reads as a setup rather than an alphabetised dump,
    /// and so a diff of two saves shows only what actually changed.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        let mut m = serde_json::Map::new();
        let put_s =
            |m: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: &Option<String>| {
                if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                    m.insert(k.into(), v.into());
                }
            };
        put_s(&mut m, "mode", &self.mode);
        put_s(&mut m, "on", &self.on);
        put_s(&mut m, "spread", &self.spread);
        if let Some(v) = self.count {
            m.insert("count".into(), v.into());
        }
        if let Some(v) = self.batch {
            m.insert("batch".into(), v.into());
        }
        put_s(&mut m, "halt_on_fail", &self.halt_on_fail);
        put_s(&mut m, "priority", &self.priority);
        put_s(&mut m, "timeout", &self.timeout);
        put_s(&mut m, "item_input", &self.item_input);
        if let Some(v) = self.gang {
            m.insert("gang".into(), v.into());
        }
        put_s(&mut m, "gang_placement", &self.gang_placement);
        if let Some(v) = self.gang_restart {
            m.insert("gang_restart".into(), v.into());
        }
        if let Some(v) = self.gpus {
            m.insert("gpus".into(), v.into());
        }
        put_s(&mut m, "vram", &self.vram);
        put_s(&mut m, "device", &self.device);
        if let Some(v) = self.gpu_share {
            m.insert("gpu_share".into(), v.into());
        }
        if let Some(v) = self.chunk {
            m.insert("chunk".into(), v.into());
        }
        if let Some(v) = self.keep_output {
            m.insert("keep_output".into(), v.into());
        }
        put_s(&mut m, "reduce", &self.reduce);
        put_s(&mut m, "reduce_on", &self.reduce_on);
        if let Some(v) = self.capture {
            m.insert("capture".into(), v.into());
        }
        put_s(&mut m, "expect", &self.expect);
        // Sorted, so two saves of the same setup produce the same bytes.
        let pairs = |v: &[(String, String)]| -> serde_json::Value {
            let mut sorted: Vec<_> = v.to_vec();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(
                sorted
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::from(v)))
                    .collect(),
            )
        };
        if !self.env.is_empty() {
            m.insert("env".into(), pairs(&self.env));
        }
        if !self.inputs.is_empty() {
            m.insert("inputs".into(), pairs(&self.inputs));
        }
        (!m.is_empty()).then_some(serde_json::Value::Object(m))
    }

    /// Read a `run` block. Anything absent, null or of the wrong shape is
    /// simply "not remembered" — this is a convenience, and a convenience
    /// that refuses to load is worse than no convenience.
    pub fn from_json(v: &serde_json::Value) -> RunDefaults {
        let Some(o) = v.as_object() else {
            return RunDefaults::default();
        };
        // `as_scalar` so `"gang": "8"` and `"gang": 8` both work: these are
        // hand-edited files, and a quoted number is the commonest slip.
        let s = |k: &str| -> Option<String> {
            o.get(k)
                .and_then(as_scalar)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let n = |k: &str| -> Option<u32> {
            o.get(k).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))
                    .map(|v| v as u32)
            })
        };
        let b = |k: &str| -> Option<bool> {
            o.get(k).and_then(|v| {
                v.as_bool().or_else(|| match v.as_str()?.trim() {
                    "true" | "yes" | "1" => Some(true),
                    "false" | "no" | "0" => Some(false),
                    _ => None,
                })
            })
        };
        // Sorted by name, as they are written. serde_json's map keeps the
        // file's order when any crate in the build enables its
        // `preserve_order` feature and sorts when none does, so without
        // this the same file read back differently depending on what it
        // was linked with.
        let pairs = |k: &str| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = o
                .get(k)
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), as_scalar(v)?)))
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        RunDefaults {
            mode: s("mode"),
            on: s("on").or_else(|| s("target")),
            spread: s("spread"),
            count: n("count"),
            batch: n("batch").or_else(|| n("max_parallel")),
            halt_on_fail: s("halt_on_fail"),
            priority: s("priority"),
            timeout: s("timeout"),
            item_input: s("item_input"),
            gang: n("gang"),
            gang_placement: s("gang_placement"),
            gang_restart: b("gang_restart"),
            gpus: n("gpus"),
            vram: s("vram"),
            device: s("device"),
            gpu_share: b("gpu_share"),
            chunk: n("chunk"),
            cores: n("cores").or_else(|| n("cpus")),
            mem: s("mem").or_else(|| s("memory")),
            disk: s("disk"),
            keep_output: b("keep_output"),
            reduce: s("reduce"),
            reduce_on: s("reduce_on"),
            capture: b("capture"),
            expect: s("expect"),
            env: pairs("env"),
            inputs: pairs("inputs"),
        }
    }
}

/// Rewrite a **contract** so it remembers how this script was set up to run.
///
/// Two edits, both in place: each named argument's `default` becomes the
/// value that was actually used, and `x-meshware.run` becomes `run`. Every
/// other keyword is left exactly as it was — a contract may carry
/// `minimum`, `pattern`, `additionalProperties` and anything else a person
/// hand-wrote, and a convenience feature has no business rewriting them.
///
/// `None` in, and a contract is built from scratch only if there is
/// something to say; a script that takes no arguments and was run with no
/// settings worth remembering stays contract-less.
///
/// Returns the contract text, or `None` when there would be nothing in it.
pub fn contract_with_defaults(
    contract: Option<&str>,
    run: &RunDefaults,
    arg_defaults: &[(String, String)],
) -> std::result::Result<Option<String>, DefinitionsError> {
    let mut schema: serde_json::Map<String, serde_json::Value> = match contract {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(text)
            .map_err(|e| DefinitionsError(format!("the contract is not valid JSON: {e}")))?,
        _ => serde_json::Map::new(),
    };
    if schema.is_empty() && run.is_empty() && arg_defaults.is_empty() {
        return Ok(None);
    }

    // Argument defaults, typed the way the schema says they are typed, so
    // `--crf 28` is saved as `28` and not `"28"`.
    if !arg_defaults.is_empty()
        && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
    {
        for (name, value) in arg_defaults {
            let Some(prop) = props.get_mut(name).and_then(|p| p.as_object_mut()) else {
                // An input the contract does not declare is not an
                // argument; it travels in `run.inputs` instead.
                continue;
            };
            let ty = match prop.get("type").and_then(|t| t.as_str()) {
                Some("integer") => ArgType::Integer,
                Some("number") => ArgType::Number,
                Some("boolean") => ArgType::Bool,
                _ => ArgType::Text,
            };
            if value.trim().is_empty() {
                // Cleared on purpose: a default of "" would make an
                // optional field look answered.
                prop.remove("default");
            } else {
                prop.insert("default".into(), coerce(value, &ty));
            }
        }
    }

    // The run block, under the extension keyword every validator ignores.
    let ext = schema
        .entry(EXTENSION)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !ext.is_object() {
        *ext = serde_json::Value::Object(serde_json::Map::new());
    }
    let ext = ext.as_object_mut().expect("just made an object");
    match run.to_json() {
        Some(v) => {
            ext.insert("run".into(), v);
        }
        None => {
            ext.remove("run");
        }
    }
    // "What one item needs" is not a run setting, it is a declaration —
    // `cores`/`mem`/`disk` already have a home here and in a definitions
    // file's `needs`. They ride in on `run` because the Run screen has one
    // form, and they are put back where they belong on the way out.
    if let Some(v) = run.cores {
        ext.insert("cores".into(), v.into());
    }
    for (k, v) in [("mem", &run.mem), ("disk", &run.disk)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            ext.insert(k.into(), v.into());
        }
    }
    if ext.is_empty() {
        schema.remove(EXTENSION);
    }
    if schema.is_empty() {
        return Ok(None);
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(schema))
        .map(Some)
        .map_err(|e| DefinitionsError(e.to_string()))
}

/// Rewrite a **definitions file** so it remembers the same thing.
///
/// `definitions` is the file as it stands — or an empty string, for a
/// script that has never had one, in which case `fallback` (what the
/// script says about itself) is the starting point rather than a blank
/// object. Nothing is invented beyond that: an argument the script never
/// declared does not appear because a run happened to pass a value for it.
pub fn definitions_with_defaults(
    definitions: &str,
    fallback: &ScriptSpec,
    run: &RunDefaults,
    arg_defaults: &[(String, String)],
) -> std::result::Result<String, DefinitionsError> {
    let mut spec = if definitions.trim().is_empty() {
        fallback.clone()
    } else {
        let mut declared = ScriptSpec::from_definitions(definitions)?;
        // A file that names no arguments but sits beside a script that
        // does should not lose them on the way through: merge the file
        // *over* what the script says, which is the precedence
        // `read_with_definitions` already uses.
        let mut merged = fallback.clone();
        merged.merge_over(std::mem::take(&mut declared));
        merged
    };
    // An inferred argument that a person has now chosen a value for is a
    // declared one: they looked at it and pressed save.
    for (name, value) in arg_defaults {
        if let Some(arg) = spec.args.iter_mut().find(|a| &a.name == name) {
            arg.inferred = false;
            arg.default = (!value.trim().is_empty()).then(|| value.clone());
        }
    }
    spec.run = run.clone();
    // As in `contract_with_defaults`: these three belong in `needs`.
    if run.cores.is_some() {
        spec.cores = run.cores;
    }
    for (slot, v) in [(&mut spec.mem, &run.mem), (&mut spec.disk, &run.disk)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            *slot = Some(v.to_string());
        }
    }
    spec.args.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(spec.to_definitions())
}

/// Resolve a script's declared `reads`/`writes` against one job's inputs.
///
/// A declaration says `reads $path`, meaning *"whatever the `path` argument
/// is set to for this run"*. The service author names the argument; the
/// submitter names the value. Anything that does not resolve to an absolute
/// path is dropped rather than guessed at — the sandbox's own
/// [`crate::sandbox::grantable`] refuses relative paths anyway, and a
/// half-substituted string is not a path.
///
/// A declaration may also name a literal path (`writes /srv/media/encoded`),
/// which is right for a service that always writes to one place.
pub fn resolve_paths(templates: &[String], inputs: &[(String, String)]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for t in templates {
        let t = t.trim();
        let resolved = match t.strip_prefix('$') {
            Some(arg) => {
                let arg = arg.trim_matches(|c| c == '{' || c == '}');
                match inputs.iter().find(|(k, _)| k == arg) {
                    Some((_, v)) if !v.trim().is_empty() => v.trim().to_string(),
                    // Not set for this run: nothing to grant, which is the
                    // correct answer rather than an error.
                    _ => continue,
                }
            }
            None => t.to_string(),
        };
        let path = std::path::PathBuf::from(&resolved);
        if path.is_absolute() {
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A default is typed the way its argument is, so the schema a person reads
/// says `28` rather than `"28"`.
fn coerce(raw: &str, ty: &ArgType) -> serde_json::Value {
    match ty {
        ArgType::Integer => raw
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| raw.into()),
        ArgType::Number => raw
            .parse::<f64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| raw.into()),
        ArgType::Bool => match raw {
            "true" | "yes" | "1" => true.into(),
            "false" | "no" | "0" => false.into(),
            other => other.into(),
        },
        _ => raw.into(),
    }
}

/// A bound, typed the way its argument is: `minimum: 0` for a whole number,
/// not `minimum: 0.0`.
fn number(v: f64, ty: &ArgType) -> serde_json::Value {
    let whole = v as i64;
    if (matches!(ty, ArgType::Integer) || v.fract() == 0.0) && whole as f64 == v {
        return whole.into();
    }
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

fn parse_needs(value: &str, spec: &mut ScriptSpec) {
    for part in value.split_whitespace() {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k {
            "cores" | "cpus" => spec.cores = v.parse().ok(),
            "mem" | "memory" => spec.mem = Some(v.to_string()),
            "disk" => spec.disk = Some(v.to_string()),
            _ => {}
        }
    }
}

/// `NAME[!] [: TYPE] [= DEFAULT] ["description"]`
fn parse_arg(value: &str) -> Option<Arg> {
    // The description is quoted and last, so take it off first and the rest
    // parses without worrying about spaces inside it.
    let (head, description) = match value.find('"') {
        Some(i) => {
            let rest = &value[i + 1..];
            let end = rest.rfind('"').unwrap_or(rest.len());
            (&value[..i], rest[..end].to_string())
        }
        None => (value, String::new()),
    };
    let head = head.trim();
    if head.is_empty() {
        return None;
    }

    let (head, default) = match head.split_once('=') {
        Some((h, d)) => (h.trim(), Some(d.trim().to_string())),
        None => (head, None),
    };
    let (name, ty) = match head.split_once(':') {
        Some((n, t)) => (n.trim(), ArgType::parse(t).unwrap_or(ArgType::Text)),
        None => (head, ArgType::Text),
    };
    let required = name.ends_with('!');
    let name = name.trim_end_matches('!').trim().trim_start_matches("--");
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(Arg {
        ty,
        default: default.filter(|d| !d.is_empty()),
        description,
        required,
        ..Arg::named(name)
    })
}

/// Arguments a bash script visibly reads, from the shape this project's own
/// scripts use and that `kernel-build.sh` established:
///
/// ```bash
/// CRF="${IN_crf:-28}"
/// CODEC="${IN_codec:-libx265}"
/// ```
///
/// Inference is a courtesy, not a contract. It finds names and defaults and
/// nothing else — no types beyond a guess from the default, no descriptions,
/// no idea which one an item fills. Everything it produces is marked
/// `inferred` so a screen can say where it came from.
pub fn infer_bash_args(source: &str) -> Vec<Arg> {
    let mut out: Vec<Arg> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("${IN_") {
            rest = &rest[i + "${IN_".len()..];
            // `name:-default}` or `name}`
            let Some(end) = rest.find('}') else { break };
            let body = &rest[..end];
            let (name, default) = match body.split_once(":-") {
                Some((n, d)) => (n, Some(d.to_string())),
                None => (body, None),
            };
            let name = name.trim();
            if name.is_empty()
                || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
                || !seen.insert(name.to_string())
            {
                rest = &rest[end..];
                continue;
            }
            let ty = match default.as_deref() {
                Some("true") | Some("false") => ArgType::Bool,
                Some(d) if d.parse::<i64>().is_ok() => ArgType::Integer,
                Some(d) if d.parse::<f64>().is_ok() => ArgType::Number,
                _ => ArgType::Text,
            };
            out.push(Arg {
                ty,
                default: default.filter(|d| !d.is_empty()),
                inferred: true,
                ..Arg::named(name)
            });
            rest = &rest[end..];
        }
    }
    out
}

// ===========================================================================
// The definitions file
// ===========================================================================
//
// A header is a comment block, and a comment block is a bad place to keep a
// data structure. `#meshware: arg codec : one of libx265|libx264 = libx265
// "Video encoder"` is a small language with its own quoting rules, invented
// to fit one line, and it stops exactly where one line stops: there is
// nowhere in it to say *"between 0 and 51"*, and nowhere to put the next
// thing after that either.
//
// So a script may instead carry a companion file — `burn-in.sh` and
// `burn-in.sh.definitions` — holding the same declaration as JSON. It is a
// separate file rather than a block inside the script because the script is
// something a machine executes and this is something a person and a GUI
// both edit; keeping them apart is what lets Director write one back out
// without ever rewriting the other.
//
// ```json
// {
//   "name": "transcode-media",
//   "about": "Re-encode one media file",
//   "item": "path",
//   "needs": { "cores": 2, "mem": "512M" },
//   "tools": ["ffmpeg", "ffprobe"],
//   "reads": ["$path"],
//   "writes": ["$outdir"],
//   "args": [
//     { "name": "path", "type": "text", "required": true,
//       "description": "The file to encode" },
//     { "name": "crf", "type": "int", "default": 28, "min": 0, "max": 51,
//       "description": "Lower is bigger" },
//     { "name": "codec", "type": "choice",
//       "choices": ["libx265", "libx264"], "default": "libx265" }
//   ]
// }
// ```
//
// Nothing here is required. A file with only `args` is a complete and
// sensible definitions file, and so is `{}`.

/// What a definitions file is called, given the script it belongs to.
/// `burn-in.sh` → `burn-in.sh.definitions`. The whole script name is kept,
/// extension and all, so `run.sh` and `run.py` do not collide.
pub const DEFINITIONS_SUFFIX: &str = ".definitions";

/// The companion path for a script path, in whatever separator it came in
/// with (bundle paths use `/`, filesystem paths use the platform's).
pub fn definitions_path(script_path: &str) -> String {
    format!("{script_path}{DEFINITIONS_SUFFIX}")
}

/// True if this path *is* a definitions file. Used to keep them out of the
/// "which file do you want to run" picker — a definitions file is never an
/// entrypoint.
pub fn is_definitions_path(path: &str) -> bool {
    path.ends_with(DEFINITIONS_SUFFIX)
}

/// Where a spec came from, so a screen can say so rather than presenting
/// three quite different levels of confidence as one filled-in form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A `.definitions` file beside the script.
    Definitions,
    /// A `#meshware:` header inside the script.
    Header,
    /// Read out of the script's own argument parsing. A guess.
    Inferred,
    /// The script said nothing and nothing could be read out of it.
    None,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Definitions => "definitions",
            Provenance::Header => "header",
            Provenance::Inferred => "inferred",
            Provenance::None => "none",
        }
    }
}

/// A definitions file that does not parse. Loud on purpose: the failure
/// this whole module exists to remove is a script quietly registering with
/// no arguments, and "your JSON has a trailing comma" silently becoming
/// that same empty form would be the identical bug wearing a new hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionsError(pub String);

impl std::fmt::Display for DefinitionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DefinitionsError {}

impl ScriptSpec {
    /// Everything a script says about itself, from both of the places it
    /// can say it.
    ///
    /// Precedence, highest first: the definitions file, the `#meshware:`
    /// header, then inference. They *merge* rather than replace — a
    /// definitions file listing only `args` still gets the header's `item`
    /// and `needs`, and an argument the definitions file does not mention
    /// keeps whatever the header or the script itself said about it. The
    /// alternative, where adding one JSON file silently deletes a header
    /// that is still sitting in the script, is a trap.
    ///
    /// `definitions` is the *content* of the companion file, or `None` when
    /// there isn't one. A malformed one is an error and never a shrug.
    pub fn read_with_definitions(
        source: &str,
        definitions: Option<&str>,
    ) -> std::result::Result<(ScriptSpec, Provenance), DefinitionsError> {
        let mut spec = ScriptSpec::read(source);
        let mut provenance = if spec.has_header() {
            Provenance::Header
        } else if spec.args.is_empty() {
            Provenance::None
        } else {
            Provenance::Inferred
        };
        if let Some(text) = definitions {
            // An empty or whitespace-only file is "no definitions", not a
            // syntax error. Touching a file is a normal way to start one.
            if !text.trim().is_empty() {
                let declared = ScriptSpec::from_definitions(text)?;
                // A file that parses but declares nothing — `{}`, or one
                // started and not yet filled in — is not the source of
                // anything, and a screen saying "read from the definitions
                // file" over fields that came from somewhere else would be
                // a lie told with a straight face.
                if !declared.is_silent() {
                    provenance = Provenance::Definitions;
                }
                spec.merge_over(declared);
            }
        }
        spec.args.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((spec, provenance))
    }

    /// Parse a definitions file on its own, with nothing merged into it.
    pub fn from_definitions(text: &str) -> std::result::Result<ScriptSpec, DefinitionsError> {
        let v: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| DefinitionsError(format!("not valid JSON: {e}")))?;
        let obj = v.as_object().ok_or_else(|| {
            DefinitionsError("the top level must be a JSON object, like { \"args\": [] }".into())
        })?;

        let mut spec = ScriptSpec {
            name: string_at(obj, &["name"]),
            about: string_at(obj, &["about", "description"]).unwrap_or_default(),
            item: string_at(obj, &["item", "each", "per_item"])
                .map(|s| s.trim_start_matches("--").to_string()),
            ..Default::default()
        };

        // `needs` as a nested object, or the same three keys at the top
        // level. Both read naturally; refusing one of them would only be a
        // rule to remember.
        let needs = obj.get("needs").and_then(|n| n.as_object());
        let pick = |k: &str| -> Option<&serde_json::Value> {
            needs.and_then(|n| n.get(k)).or_else(|| obj.get(k))
        };
        spec.cores = pick("cores")
            .or_else(|| pick("cpus"))
            .and_then(|c| c.as_u64())
            .map(|c| c as u32);
        spec.mem = pick("mem").or_else(|| pick("memory")).and_then(as_scalar);
        spec.disk = pick("disk").and_then(as_scalar);

        spec.reads = string_list(obj.get("reads"));
        spec.writes = string_list(obj.get("writes"));
        spec.tools = string_list(obj.get("tools").or_else(|| obj.get("needs_tools")));
        // How this script was last set up to run. A wrong *shape* here is
        // refused with the rest of the file rather than shrugged at — a
        // `"run": []` that silently means "no defaults" is the same trap
        // as a trailing comma silently meaning "no arguments".
        match obj.get("run") {
            Some(serde_json::Value::Object(_)) => {
                spec.run = RunDefaults::from_json(&obj["run"]);
            }
            None | Some(serde_json::Value::Null) => {}
            Some(_) => {
                return Err(DefinitionsError(
                    "\"run\" must be an object of run settings, like { \"mode\": \"once\" }".into(),
                ));
            }
        }

        match obj.get("args") {
            // The canonical form: an array, because arguments have an order
            // and a form should show them in the one the author chose.
            Some(serde_json::Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    let o = item
                        .as_object()
                        .ok_or_else(|| DefinitionsError(format!("args[{i}] must be an object")))?;
                    let name = string_at(o, &["name", "arg"])
                        .ok_or_else(|| DefinitionsError(format!("args[{i}] has no \"name\"")))?;
                    spec.args.push(parse_definition_arg(&name, o)?);
                }
            }
            // The convenient form: a map of name to definition.
            Some(serde_json::Value::Object(map)) => {
                for (name, item) in map {
                    let o = item.as_object().ok_or_else(|| {
                        DefinitionsError(format!("args.{name} must be an object"))
                    })?;
                    spec.args.push(parse_definition_arg(name, o)?);
                }
            }
            None | Some(serde_json::Value::Null) => {}
            Some(_) => {
                return Err(DefinitionsError(
                    "\"args\" must be an array of objects, or an object keyed by name".into(),
                ));
            }
        }

        for (name, count) in duplicate_names(&spec.args) {
            if count > 1 {
                return Err(DefinitionsError(format!(
                    "\"{name}\" is defined {count} times"
                )));
            }
        }
        // Keys this build does not know are kept, not refused, for the same
        // reason an unknown header directive is: a definitions file written
        // for a later version should still register on this one.
        for (k, v) in obj {
            if !DEFINITION_KEYS.contains(&k.as_str()) {
                spec.unknown.push(k.clone());
                spec.extra.insert(k.clone(), v.clone());
            }
        }
        Ok(spec)
    }

    /// True when this spec declares nothing at all.
    fn is_silent(&self) -> bool {
        self.name.is_none()
            && self.about.is_empty()
            && self.item.is_none()
            && self.cores.is_none()
            && self.mem.is_none()
            && self.disk.is_none()
            && self.reads.is_empty()
            && self.writes.is_empty()
            && self.tools.is_empty()
            && self.args.is_empty()
            && self.run.is_empty()
    }

    /// Take everything `other` declares, keeping what it is silent about.
    fn merge_over(&mut self, other: ScriptSpec) {
        if other.name.is_some() {
            self.name = other.name;
        }
        if !other.about.is_empty() {
            self.about = other.about;
        }
        if other.item.is_some() {
            self.item = other.item;
        }
        if other.cores.is_some() {
            self.cores = other.cores;
        }
        if other.mem.is_some() {
            self.mem = other.mem;
        }
        if other.disk.is_some() {
            self.disk = other.disk;
        }
        if !other.reads.is_empty() {
            self.reads = other.reads;
        }
        if !other.writes.is_empty() {
            self.writes = other.writes;
        }
        if !other.tools.is_empty() {
            self.tools = other.tools;
        }
        self.run.merge_over(other.run);
        self.unknown.extend(other.unknown);
        self.extra.extend(other.extra);
        for arg in other.args {
            match self.args.iter_mut().find(|a| a.name == arg.name) {
                Some(existing) => *existing = arg,
                None => self.args.push(arg),
            }
        }
    }

    /// Write this spec out as a definitions file.
    ///
    /// The point of the direction is Director: a script that carries only a
    /// header, or only the arguments that could be read out of it, can be
    /// opened, corrected in a form, and saved as a definitions file that
    /// says what the person actually meant. Nothing is invented — an
    /// inferred argument is written out the same as a declared one, because
    /// once a person has looked at it and pressed save, it *is* declared.
    pub fn to_definitions(&self) -> String {
        let needs = DefinitionsNeeds {
            cores: self.cores,
            mem: self.mem.clone(),
            disk: self.disk.clone(),
        };
        let file = DefinitionsFile {
            name: self.name.clone(),
            about: (!self.about.is_empty()).then(|| self.about.clone()),
            item: self.item.clone(),
            needs: (!needs.is_empty()).then_some(needs),
            tools: self.tools.clone(),
            reads: self.reads.clone(),
            writes: self.writes.clone(),
            run: self.run.to_json(),
            extra: self.extra.clone(),
            args: self
                .args
                .iter()
                .map(|a| DefinitionsArg {
                    name: a.name.clone(),
                    ty: a.ty.definitions_name(),
                    choices: match &a.ty {
                        ArgType::Choice(c) => c.clone(),
                        _ => Vec::new(),
                    },
                    required: a.required,
                    default: a.default.as_ref().map(|d| coerce(d, &a.ty)),
                    min: a.min.map(|v| number(v, &a.ty)),
                    max: a.max.map(|v| number(v, &a.ty)),
                    pattern: a.pattern.clone(),
                    description: (!a.description.is_empty()).then(|| a.description.clone()),
                })
                .collect(),
        };
        // Pretty and newline-terminated: this is a file a person opens in an
        // editor and a diff shows line by line.
        let mut out = serde_json::to_string_pretty(&file).unwrap_or_else(|_| "{}".to_string());
        out.push('\n');
        out
    }
}

// The shape of the file on the way *out*. A struct rather than a
// `serde_json::Map` because serde writes a struct's fields in the order they
// are declared and sorts a map's keys alphabetically — and a file whose
// first line is `"about"` and whose arguments each begin with `"choices"` is
// a file that reads like a data dump instead of a declaration. Reading is
// separate, in `from_definitions`, which is forgiving about key order,
// spelling and shape in a way a derived `Deserialize` would not be.
#[derive(serde::Serialize)]
struct DefinitionsFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs: Option<DefinitionsNeeds>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reads: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    writes: Vec<String>,
    /// How this script was last set up to run. Written *before* `args`
    /// because it is the half a person comes back to edit — the arguments
    /// were settled the first time and the run settings are what somebody
    /// tunes on the second afternoon.
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<serde_json::Value>,
    /// Always written, even when empty: `"args": []` is how a file says
    /// *"this script takes nothing"*, and it is the line a person edits
    /// first when that turns out to be wrong.
    args: Vec<DefinitionsArg>,
    /// Keys this build does not know, written back exactly as they came
    /// in. Flattened and declared last, so they land after everything
    /// above rather than being sorted in among it.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(serde::Serialize)]
struct DefinitionsNeeds {
    #[serde(skip_serializing_if = "Option::is_none")]
    cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: Option<String>,
}

impl DefinitionsNeeds {
    fn is_empty(&self) -> bool {
        self.cores.is_none() && self.mem.is_none() && self.disk.is_none()
    }
}

#[derive(serde::Serialize)]
struct DefinitionsArg {
    name: String,
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    choices: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// Top-level keys this build understands. Anything else is carried in
/// `unknown` rather than refused.
const DEFINITION_KEYS: &[&str] = &[
    "name",
    "about",
    "description",
    "item",
    "each",
    "per_item",
    "needs",
    "cores",
    "cpus",
    "mem",
    "memory",
    "disk",
    "reads",
    "writes",
    "tools",
    "needs_tools",
    "args",
    "run",
];

impl ArgType {
    /// The name this type is written with in a definitions file. The short
    /// spellings the header uses are all still accepted on the way in.
    fn definitions_name(&self) -> &'static str {
        match self {
            ArgType::Text => "text",
            ArgType::Integer => "int",
            ArgType::Number => "number",
            ArgType::Bool => "bool",
            ArgType::Choice(_) => "choice",
        }
    }
}

fn parse_definition_arg(
    name: &str,
    o: &serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<Arg, DefinitionsError> {
    let name = name.trim().trim_start_matches("--").to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(DefinitionsError(format!(
            "\"{name}\" is not a usable argument name — letters, digits, `_` and `-` only"
        )));
    }

    let choices = string_list(
        o.get("choices")
            .or_else(|| o.get("enum"))
            .or_else(|| o.get("options")),
    );
    let declared = string_at(o, &["type", "kind"]);
    let ty = match declared.as_deref() {
        // "choice" carries its options in a separate key, so it is the one
        // spelling `ArgType::parse` cannot resolve on its own.
        Some("choice") | Some("one of") | Some("enum") => {
            if choices.is_empty() {
                return Err(DefinitionsError(format!(
                    "\"{name}\" is a choice but lists no \"choices\""
                )));
            }
            ArgType::Choice(choices)
        }
        Some(t) => ArgType::parse(t).ok_or_else(|| {
            DefinitionsError(format!(
                "\"{name}\" has unknown type \"{t}\" — use text, int, number, bool or choice"
            ))
        })?,
        // No type given: a list of choices is a choice, and everything else
        // is text until a default says otherwise.
        None if !choices.is_empty() => ArgType::Choice(choices),
        None => match o.get("default") {
            Some(serde_json::Value::Bool(_)) => ArgType::Bool,
            Some(serde_json::Value::Number(n)) if n.is_i64() || n.is_u64() => ArgType::Integer,
            Some(serde_json::Value::Number(_)) => ArgType::Number,
            _ => ArgType::Text,
        },
    };

    let default = o.get("default").and_then(as_scalar);
    if let (ArgType::Choice(c), Some(d)) = (&ty, default.as_deref())
        && !c.iter().any(|x| x == d)
    {
        return Err(DefinitionsError(format!(
            "\"{name}\" defaults to \"{d}\", which is not one of its choices"
        )));
    }

    let num = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| o.get(*k)).and_then(|v| v.as_f64())
    };
    Ok(Arg {
        ty,
        default,
        description: string_at(o, &["description", "about", "help"]).unwrap_or_default(),
        required: o.get("required").and_then(|r| r.as_bool()).unwrap_or(false),
        min: num(&["min", "minimum"]),
        max: num(&["max", "maximum"]),
        pattern: string_at(o, &["pattern", "matches"]),
        ..Arg::named(name)
    })
}

/// The first of several spellings that is present and a non-empty string.
fn string_at(o: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| o.get(*k))
        .and_then(as_scalar)
        .filter(|s| !s.is_empty())
}

/// A JSON scalar as the string the rest of this module stores. `28` and
/// `"28"` mean the same thing here; `coerce` types it back on the way out.
fn as_scalar(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A list of strings, or a single string, or a comma-separated one — all
/// three are things people write, and none of them is ambiguous.
fn string_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(as_scalar).collect(),
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn duplicate_names(args: &[Arg]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for a in args {
        *counts.entry(a.name.clone()).or_default() += 1;
    }
    counts.into_iter().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // The header the transcode workload would carry — the same script whose
    // registration produced "None declared" and a Run screen with no fields.
    const TRANSCODE: &str = r#"#!/usr/bin/env bash
#meshware: name transcode-media
#meshware: about Re-encode one media file
#meshware: item path
#meshware: needs cores=2 mem=512M
#meshware: reads $path
#meshware: writes $outdir
#meshware: tools ffmpeg, ffprobe
#meshware: arg path!                                      "The file to encode"
#meshware: arg codec : one of libx265|libx264|libsvtav1 = libx265  "Video encoder"
#meshware: arg crf : int = 28                             "Lower is bigger"
#meshware: arg strip : bool = true                        "Remove metadata"
set -uo pipefail
"#;

    #[test]
    fn a_header_says_everything_a_run_screen_needs() {
        let spec = ScriptSpec::read(TRANSCODE);
        assert_eq!(spec.name.as_deref(), Some("transcode-media"));
        assert_eq!(spec.about, "Re-encode one media file");
        assert_eq!(spec.item.as_deref(), Some("path"));
        assert_eq!(spec.cores, Some(2));
        assert_eq!(spec.mem.as_deref(), Some("512M"));
        assert_eq!(spec.reads, vec!["$path"]);
        assert_eq!(spec.writes, vec!["$outdir"]);
        assert_eq!(spec.tools, vec!["ffmpeg", "ffprobe"]);
        assert!(spec.has_header());

        let by = |n: &str| spec.args.iter().find(|a| a.name == n).expect(n).clone();
        let path = by("path");
        assert!(path.required);
        assert_eq!(path.description, "The file to encode");

        let codec = by("codec");
        assert_eq!(
            codec.ty,
            ArgType::Choice(vec!["libx265".into(), "libx264".into(), "libsvtav1".into()])
        );
        assert_eq!(codec.default.as_deref(), Some("libx265"));

        assert_eq!(by("crf").ty, ArgType::Integer);
        assert_eq!(by("crf").default.as_deref(), Some("28"));
        assert_eq!(by("strip").ty, ArgType::Bool);
        assert!(!by("strip").inferred);
    }

    #[test]
    fn the_contract_is_the_shape_director_can_edit() {
        let json = ScriptSpec::read(TRANSCODE)
            .contract_json()
            .expect("contract");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(v["type"], "object");
        assert_eq!(v["additionalProperties"], false);
        assert_eq!(v["required"][0], "path");
        // Typed defaults, not stringified ones.
        assert_eq!(v["properties"]["crf"]["type"], "integer");
        assert_eq!(v["properties"]["crf"]["default"], 28);
        assert_eq!(v["properties"]["strip"]["default"], true);
        assert_eq!(v["properties"]["codec"]["enum"][0], "libx265");
        // And it compiles as a JSON Schema, which is what `service create`
        // checks before it will register anything.
        jsonschema::validator_for(&v).expect("valid schema");
    }

    #[test]
    fn the_non_argument_half_of_the_header_survives_the_round_trip() {
        let spec = ScriptSpec::read(TRANSCODE);
        let json = spec.contract_json().expect("contract");
        // Still a valid schema: the extension keyword is one validators ignore.
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        jsonschema::validator_for(&v).expect("valid schema");

        let back = ScriptSpec::from_contract(&json);
        assert_eq!(back.item.as_deref(), Some("path"));
        assert_eq!(back.cores, Some(2));
        assert_eq!(back.mem.as_deref(), Some("512M"));
        assert_eq!(back.reads, vec!["$path"]);
        assert_eq!(back.writes, vec!["$outdir"]);
        assert_eq!(back.tools, vec!["ffmpeg", "ffprobe"]);
        assert_eq!(back.about, "Re-encode one media file");
    }

    #[test]
    fn declared_paths_resolve_against_the_run_that_used_them() {
        let inputs = vec![
            ("path".to_string(), "/srv/media/in.mkv".to_string()),
            ("outdir".to_string(), "/srv/media/encoded".to_string()),
            ("empty".to_string(), "  ".to_string()),
        ];
        let got = resolve_paths(
            &[
                "$path".into(),
                "${outdir}".into(),
                "/etc/ssl/certs".into(),
                "$empty".into(),
                "$never_set".into(),
                "relative/dir".into(),
            ],
            &inputs,
        );
        let got: Vec<String> = got.iter().map(|p| p.display().to_string()).collect();
        // Substituted, literal, sorted, deduped. An argument that is unset
        // or blank grants nothing; a relative path is not a path.
        assert_eq!(
            got,
            vec!["/etc/ssl/certs", "/srv/media/encoded", "/srv/media/in.mkv"]
        );
    }

    #[test]
    fn a_contract_written_before_this_existed_still_reads() {
        // Every service registered by hand, or by an older build.
        let plain = r#"{"type":"object","properties":{"a":{"type":"string"}}}"#;
        let back = ScriptSpec::from_contract(plain);
        assert_eq!(back.item, None);
        assert_eq!(back.cores, None);
        assert!(back.reads.is_empty());
        // And nonsense is "not declared", never a panic.
        assert_eq!(ScriptSpec::from_contract("not json").cores, None);
    }

    #[test]
    fn a_script_with_no_header_still_gets_fields() {
        // The house parser shape, from `kernel-build.sh`.
        let src = r#"#!/usr/bin/env bash
CONFIG="${IN_config:-defconfig}"
JOBS="${IN_jobs:-auto}"
CHECK_ONLY="${IN_check:-false}"
CRF="${IN_crf:-28}"
TAG="${IN_tag:-}"
# a comment mentioning ${IN_ignored:-x} is not a declaration
"#;
        let spec = ScriptSpec::read(src);
        let names: Vec<&str> = spec.args.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["check", "config", "crf", "jobs", "tag"]);
        assert!(spec.args.iter().all(|a| a.inferred));
        assert!(!spec.has_header());
        let by = |n: &str| spec.args.iter().find(|a| a.name == n).expect(n).clone();
        assert_eq!(by("check").ty, ArgType::Bool);
        assert_eq!(by("crf").ty, ArgType::Integer);
        assert_eq!(by("config").default.as_deref(), Some("defconfig"));
        // No default written is not the same as a default of "".
        assert_eq!(by("tag").default, None);
    }

    #[test]
    fn a_declaration_beats_an_inference_for_the_same_name() {
        let src = r#"#meshware: arg crf : int = 20   "Quality"
CRF="${IN_crf:-28}"
OTHER="${IN_other:-x}"
"#;
        let spec = ScriptSpec::read(src);
        let crf = spec.args.iter().find(|a| a.name == "crf").expect("crf");
        assert_eq!(crf.default.as_deref(), Some("20"));
        assert_eq!(crf.description, "Quality");
        assert!(!crf.inferred);
        // The undeclared one is still found, and still marked.
        assert!(
            spec.args
                .iter()
                .find(|a| a.name == "other")
                .expect("other")
                .inferred
        );
    }

    #[test]
    fn an_unknown_directive_is_kept_not_refused() {
        // A script written for a later build must still register here.
        let spec = ScriptSpec::from_header("#meshware: gpus 2\n#meshware: name x\n");
        assert_eq!(spec.name.as_deref(), Some("x"));
        assert_eq!(spec.unknown, vec!["gpus 2"]);
    }

    #[test]
    fn no_arguments_means_no_contract_rather_than_an_empty_one() {
        assert!(ScriptSpec::read("echo hello\n").contract_json().is_none());
    }

    // -- the definitions file ------------------------------------------------

    const DEFS: &str = r#"{
      "name": "transcode-media",
      "about": "Re-encode one media file",
      "item": "path",
      "needs": { "cores": 2, "mem": "512M" },
      "tools": ["ffmpeg", "ffprobe"],
      "reads": ["$path"],
      "writes": ["$outdir"],
      "args": [
        { "name": "path", "type": "text", "required": true,
          "description": "The file to encode" },
        { "name": "crf", "type": "int", "default": 28, "min": 0, "max": 51,
          "description": "Lower is bigger" },
        { "name": "codec", "type": "choice",
          "choices": ["libx265", "libx264"], "default": "libx265" },
        { "name": "strip", "default": true },
        { "name": "tag", "pattern": "^[a-z0-9-]+$" }
      ]
    }"#;

    #[test]
    fn a_definitions_file_says_everything_a_header_says_and_more() {
        let (spec, from) =
            ScriptSpec::read_with_definitions("echo hi\n", Some(DEFS)).expect("defs");
        assert_eq!(from, Provenance::Definitions);
        assert_eq!(spec.name.as_deref(), Some("transcode-media"));
        assert_eq!(spec.item.as_deref(), Some("path"));
        assert_eq!(spec.cores, Some(2));
        assert_eq!(spec.mem.as_deref(), Some("512M"));
        assert_eq!(spec.tools, vec!["ffmpeg", "ffprobe"]);
        assert_eq!(spec.reads, vec!["$path"]);

        let by = |n: &str| spec.args.iter().find(|a| a.name == n).expect(n).clone();
        assert!(by("path").required);
        assert_eq!(by("crf").ty, ArgType::Integer);
        assert_eq!(by("crf").min, Some(0.0));
        assert_eq!(by("crf").max, Some(51.0));
        assert_eq!(
            by("codec").ty,
            ArgType::Choice(vec!["libx265".into(), "libx264".into()])
        );
        // No "type" given: the default's own JSON type settles it, which is
        // the whole reason defaults are typed here and stringly in a header.
        assert_eq!(by("strip").ty, ArgType::Bool);
        assert_eq!(by("tag").pattern.as_deref(), Some("^[a-z0-9-]+$"));
        // Nothing a definitions file declares is a guess.
        assert!(spec.args.iter().all(|a| !a.inferred));
    }

    #[test]
    fn bounds_reach_the_contract_as_the_keywords_that_enforce_them() {
        let (spec, _) = ScriptSpec::read_with_definitions("", Some(DEFS)).expect("defs");
        let json = spec.contract_json().expect("contract");
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        // Whole numbers, not 0.0 and 51.0 — this file is read by people.
        assert_eq!(v["properties"]["crf"]["minimum"], 0);
        assert_eq!(v["properties"]["crf"]["maximum"], 51);
        assert_eq!(v["properties"]["tag"]["pattern"], "^[a-z0-9-]+$");
        // And it is still a schema the engine will accept.
        jsonschema::validator_for(&v).expect("valid schema");
    }

    #[test]
    fn a_definitions_file_wins_but_does_not_erase_the_rest() {
        // A script with a header that still says the things the JSON file is
        // silent about. Adding the file must not delete them.
        let src = r#"#meshware: name from-header
#meshware: item path
#meshware: needs cores=4
#meshware: arg crf : int = 28   "From the header"
#meshware: arg keep : bool = true
OTHER="${IN_other:-x}"
"#;
        let defs = r#"{ "args": [ { "name": "crf", "type": "int", "default": 20 } ] }"#;
        let (spec, from) = ScriptSpec::read_with_definitions(src, Some(defs)).expect("defs");
        assert_eq!(from, Provenance::Definitions);
        // Declared in both: the file wins, wholly — including clearing the
        // description it does not repeat.
        let crf = spec.args.iter().find(|a| a.name == "crf").expect("crf");
        assert_eq!(crf.default.as_deref(), Some("20"));
        assert_eq!(crf.description, "");
        // Declared only in the header, and only in the script: both kept.
        assert!(spec.args.iter().any(|a| a.name == "keep" && !a.inferred));
        assert!(spec.args.iter().any(|a| a.name == "other" && a.inferred));
        assert_eq!(spec.name.as_deref(), Some("from-header"));
        assert_eq!(spec.item.as_deref(), Some("path"));
        assert_eq!(spec.cores, Some(4));
    }

    #[test]
    fn a_broken_definitions_file_is_an_error_not_an_empty_form() {
        // The bug this module exists to remove is a script registering with
        // no arguments and no explanation. A typo in the JSON must not be a
        // new way to arrive there.
        let cases = [
            ("{ \"args\": [ ] ", "not valid JSON"),
            ("[]", "top level must be a JSON object"),
            ("{\"args\": 3}", "must be an array of objects"),
            ("{\"args\": [{\"type\": \"int\"}]}", "has no \"name\""),
            (
                "{\"args\": [{\"name\": \"a\", \"type\": \"colour\"}]}",
                "unknown type",
            ),
            (
                "{\"args\": [{\"name\": \"a\", \"type\": \"choice\"}]}",
                "lists no \"choices\"",
            ),
            (
                "{\"args\": [{\"name\": \"a\", \"choices\": [\"x\"], \"default\": \"y\"}]}",
                "not one of its choices",
            ),
            (
                "{\"args\": [{\"name\": \"a\"}, {\"name\": \"a\"}]}",
                "defined 2 times",
            ),
        ];
        for (text, expect) in cases {
            let err = ScriptSpec::from_definitions(text).expect_err(text);
            assert!(
                err.0.contains(expect),
                "{text}\n  wanted a message containing {expect:?}\n  got {:?}",
                err.0
            );
        }
    }

    #[test]
    fn an_empty_definitions_file_is_not_a_broken_one() {
        // Two normal ways to start one: `touch`, and `echo {} >`.
        for text in ["", "   \n", "{}"] {
            let (spec, _) =
                ScriptSpec::read_with_definitions("CRF=\"${IN_crf:-28}\"\n", Some(text))
                    .expect(text);
            assert!(spec.args.iter().any(|a| a.name == "crf"));
        }
    }

    #[test]
    fn a_definitions_file_round_trips_through_the_editor_and_back() {
        // Director reads one, a person edits a form, Director writes one.
        // What comes out must mean what went in, or editing an argument is
        // a way to lose the rest.
        let (spec, _) = ScriptSpec::read_with_definitions("", Some(DEFS)).expect("defs");
        let written = spec.to_definitions();
        let (again, from) = ScriptSpec::read_with_definitions("", Some(&written)).expect("reread");
        assert_eq!(from, Provenance::Definitions);
        assert_eq!(again.args, spec.args);
        assert_eq!(again.name, spec.name);
        assert_eq!(again.item, spec.item);
        assert_eq!(again.cores, spec.cores);
        assert_eq!(again.mem, spec.mem);
        assert_eq!(again.tools, spec.tools);
        assert_eq!(again.reads, spec.reads);
        assert_eq!(again.writes, spec.writes);
        assert!(written.ends_with('\n'));
    }

    #[test]
    fn a_header_becomes_a_definitions_file_without_losing_anything() {
        // The upgrade path: a script that only has a header, saved from
        // Director as JSON. What it declared has to survive the crossing.
        let spec = ScriptSpec::read(TRANSCODE);
        let (back, _) =
            ScriptSpec::read_with_definitions("", Some(&spec.to_definitions())).expect("defs");
        assert_eq!(back.args, spec.args);
        assert_eq!(back.item, spec.item);
        assert_eq!(back.cores, spec.cores);
        assert_eq!(back.about, spec.about);
    }

    #[test]
    fn args_may_be_a_map_when_that_reads_better() {
        let defs = r#"{ "args": { "crf": { "type": "int", "default": 28 },
                                  "path": { "required": true } } }"#;
        let spec = ScriptSpec::from_definitions(defs).expect("defs");
        let by = |n: &str| spec.args.iter().find(|a| a.name == n).expect(n).clone();
        assert_eq!(by("crf").ty, ArgType::Integer);
        assert!(by("path").required);
    }

    #[test]
    fn provenance_says_which_of_the_three_answers_this_is() {
        let with_defs = ScriptSpec::read_with_definitions("", Some("{}")).unwrap().1;
        // `{}` declares nothing, so it is not the source of anything.
        assert_eq!(with_defs, Provenance::None);
        assert_eq!(
            ScriptSpec::read_with_definitions(TRANSCODE, None)
                .unwrap()
                .1,
            Provenance::Header
        );
        assert_eq!(
            ScriptSpec::read_with_definitions("A=\"${IN_a:-1}\"\n", None)
                .unwrap()
                .1,
            Provenance::Inferred
        );
        assert_eq!(
            ScriptSpec::read_with_definitions("echo hi\n", None)
                .unwrap()
                .1,
            Provenance::None
        );
    }

    #[test]
    fn a_definitions_file_is_named_after_the_whole_script_name() {
        // `run.sh` and `run.py` in one directory must not share one file.
        assert_eq!(definitions_path("run.sh"), "run.sh.definitions");
        assert_eq!(definitions_path("a/b/run.py"), "a/b/run.py.definitions");
        assert!(is_definitions_path("run.sh.definitions"));
        assert!(!is_definitions_path("run.sh"));
    }

    // -- run defaults ------------------------------------------------------

    #[test]
    fn run_defaults_round_trip_through_the_file() {
        let src = r#"{
          "args": [{ "name": "path", "type": "text", "required": true }],
          "run": {
            "mode": "peritem", "on": "tag=gpu", "spread": "all",
            "priority": "high", "timeout": "45m", "item_input": "path",
            "gpus": 2, "vram": "24G", "gpu_share": false,
            "chunk": 8, "keep_output": true, "capture": true,
            "env": { "B": "2", "A": "1" }
          }
        }"#;
        let spec = ScriptSpec::from_definitions(src).expect("defs");
        assert_eq!(spec.run.mode.as_deref(), Some("peritem"));
        assert_eq!(spec.run.on.as_deref(), Some("tag=gpu"));
        assert_eq!(spec.run.gpus, Some(2));
        assert_eq!(spec.run.keep_output, Some(true));
        // Absent is "not remembered", never "off": `gang_restart` was not
        // written, so nothing about it may be asserted on the way out.
        assert_eq!(spec.run.gang_restart, None);
        assert_eq!(spec.run.gpu_share, Some(false));

        // Written out and read back is the same block, and its env is
        // sorted so two saves of one setup are byte-identical.
        let again = ScriptSpec::from_definitions(&spec.to_definitions()).expect("round trip");
        assert_eq!(again.run, spec.run);
        assert!(spec.to_definitions().contains("\"A\": \"1\""));
        assert_eq!(spec.to_definitions(), again.to_definitions());
    }

    #[test]
    fn run_defaults_travel_in_the_contract_so_another_machine_sees_them() {
        // The point of putting them in the contract as well as the file:
        // the file is on one laptop, the contract is replicated.
        let mut spec = ScriptSpec::from_definitions(
            r#"{ "args": [{"name":"n","type":"int"}],
                 "run": { "mode": "together", "gang": 8, "gang_placement": "pack" } }"#,
        )
        .expect("defs");
        spec.about = "train".into();
        let contract = spec.contract_json().expect("contract");
        let back = ScriptSpec::from_contract(&contract);
        assert_eq!(back.run.mode.as_deref(), Some("together"));
        assert_eq!(back.run.gang, Some(8));
        assert_eq!(back.run.gang_placement.as_deref(), Some("pack"));
    }

    #[test]
    fn a_script_with_no_run_block_carries_none_and_writes_none() {
        let spec = ScriptSpec::from_definitions(r#"{ "args": [] }"#).expect("defs");
        assert!(spec.run.is_empty());
        // Nothing invented: a file that never mentioned run settings does
        // not grow a `"run": {}` just for having been opened.
        assert!(!spec.to_definitions().contains("\"run\""));
        assert!(spec.contract_json().is_none_or(|c| !c.contains("\"run\"")));
    }

    #[test]
    fn a_run_block_of_the_wrong_shape_is_an_error_not_a_shrug() {
        let err = ScriptSpec::from_definitions(r#"{ "run": ["mode"] }"#).expect_err("refused");
        assert!(err.0.contains("run"), "{}", err.0);
        // …but a null one is simply absent, as an unset key should be.
        let ok = ScriptSpec::from_definitions(r#"{ "run": null }"#).expect("null run");
        assert!(ok.run.is_empty());
    }

    #[test]
    fn a_run_block_merges_over_the_header_rather_than_replacing_it() {
        // Half a block must leave the other half alone — the same rule the
        // rest of the merge follows.
        let mut base = RunDefaults {
            timeout: Some("10m".into()),
            priority: Some("low".into()),
            ..Default::default()
        };
        base.merge_over(RunDefaults {
            priority: Some("high".into()),
            ..Default::default()
        });
        assert_eq!(base.timeout.as_deref(), Some("10m"));
        assert_eq!(base.priority.as_deref(), Some("high"));
    }

    #[test]
    fn a_hand_edited_number_in_quotes_still_reads() {
        // These files are edited by people, and `"gang": "8"` is the
        // commonest slip there is. Refusing it would teach nothing.
        let spec = ScriptSpec::from_definitions(r#"{ "run": { "gang": "8", "capture": "yes" } }"#)
            .expect("defs");
        assert_eq!(spec.run.gang, Some(8));
        assert_eq!(spec.run.capture, Some(true));
    }

    #[test]
    fn a_file_that_only_sets_run_defaults_is_not_silent() {
        // Provenance has to say "definitions", or the screen would credit
        // the header for settings the header never mentioned.
        let (spec, prov) = ScriptSpec::read_with_definitions(
            "echo hi\n",
            Some(r#"{ "run": { "priority": "high" } }"#),
        )
        .expect("read");
        assert_eq!(prov, Provenance::Definitions);
        assert_eq!(spec.run.priority.as_deref(), Some("high"));
    }

    #[test]
    fn what_one_item_needs_lands_in_needs_and_not_in_the_run_block() {
        // The Run screen has one form, so cores/mem/disk arrive on `run`.
        // They must come to rest where they already live, or a form and a
        // file end up holding two answers to one question.
        let run = RunDefaults {
            cores: Some(4),
            mem: Some("8G".into()),
            priority: Some("high".into()),
            ..Default::default()
        };
        let text = definitions_with_defaults("", &ScriptSpec::default(), &run, &[]).expect("defs");
        let back = ScriptSpec::from_definitions(&text).expect("reparse");
        assert_eq!(back.cores, Some(4));
        assert_eq!(back.mem.as_deref(), Some("8G"));
        assert_eq!(back.run.cores, None, "not stored twice: {text}");
        assert_eq!(back.run.priority.as_deref(), Some("high"));

        let contract = contract_with_defaults(None, &run, &[])
            .expect("ok")
            .expect("some");
        let spec = ScriptSpec::from_contract(&contract);
        assert_eq!(spec.cores, Some(4));
        assert_eq!(spec.mem.as_deref(), Some("8G"));
        assert_eq!(spec.run.cores, None);
    }

    #[test]
    fn saving_defaults_sets_the_arguments_own_defaults_and_leaves_the_rest() {
        let contract = r#"{
          "type": "object",
          "properties": {
            "crf": { "type": "integer", "minimum": 0, "description": "quality" },
            "tag": { "type": "string" }
          },
          "required": ["crf"],
          "additionalProperties": false
        }"#;
        let saved = contract_with_defaults(
            Some(contract),
            &RunDefaults {
                priority: Some("low".into()),
                ..Default::default()
            },
            &[("crf".into(), "28".into()), ("tag".into(), "".into())],
        )
        .expect("ok")
        .expect("some");
        let v: serde_json::Value = serde_json::from_str(&saved).expect("json");
        // Typed the way the schema says, not as a string.
        assert_eq!(v["properties"]["crf"]["default"], serde_json::json!(28));
        // Everything hand-written about it is still there.
        assert_eq!(v["properties"]["crf"]["minimum"], serde_json::json!(0));
        assert_eq!(
            v["properties"]["crf"]["description"],
            serde_json::json!("quality")
        );
        assert_eq!(v["required"], serde_json::json!(["crf"]));
        assert_eq!(v["additionalProperties"], serde_json::json!(false));
        // A cleared value removes the default rather than defaulting to "".
        assert!(v["properties"]["tag"].get("default").is_none());
        assert_eq!(v[EXTENSION]["run"]["priority"], serde_json::json!("low"));
    }

    #[test]
    fn saving_defaults_keeps_a_key_this_build_does_not_know() {
        // The file is rewritten as a side effect of pressing Run, so a key
        // somebody hand-wrote must survive a build that has never heard
        // of it.
        let existing = r#"{ "args": [{"name":"p","type":"text"}], "sbom": {"spdx": "x"} }"#;
        let text = definitions_with_defaults(
            existing,
            &ScriptSpec::default(),
            &RunDefaults {
                timeout: Some("5m".into()),
                ..Default::default()
            },
            &[("p".into(), "/srv".into())],
        )
        .expect("defs");
        let v: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(v["sbom"]["spdx"], serde_json::json!("x"));
        assert_eq!(v["run"]["timeout"], serde_json::json!("5m"));
        assert_eq!(v["args"][0]["default"], serde_json::json!("/srv"));
    }

    #[test]
    fn a_first_save_starts_from_what_the_script_already_said() {
        // No definitions file yet: the result must be the whole
        // declaration, not a file holding only the run block.
        let fallback = ScriptSpec::read(
            "#meshware: name burn-in\n#meshware: arg minutes : int = 5   \"How long\"\n",
        );
        let text = definitions_with_defaults(
            "",
            &fallback,
            &RunDefaults {
                mode: Some("once".into()),
                ..Default::default()
            },
            &[("minutes".into(), "30".into())],
        )
        .expect("defs");
        let back = ScriptSpec::from_definitions(&text).expect("reparse");
        assert_eq!(back.name.as_deref(), Some("burn-in"));
        let minutes = back
            .args
            .iter()
            .find(|a| a.name == "minutes")
            .expect("minutes");
        assert_eq!(minutes.default.as_deref(), Some("30"));
        assert_eq!(minutes.description, "How long");
        assert_eq!(back.run.mode.as_deref(), Some("once"));
    }

    #[test]
    fn nothing_to_remember_writes_no_contract_at_all() {
        assert_eq!(
            contract_with_defaults(None, &RunDefaults::default(), &[]).expect("ok"),
            None
        );
    }

    #[test]
    fn a_key_from_a_later_version_is_kept_not_refused() {
        let spec = ScriptSpec::from_definitions(r#"{ "gpus": 2, "name": "x" }"#).expect("defs");
        assert_eq!(spec.name.as_deref(), Some("x"));
        assert_eq!(spec.unknown, vec!["gpus"]);
    }
}
