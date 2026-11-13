//! Facts: collecting them, canonicalizing them, and deciding when to
//! publish (M18 §4).
//!
//! The wire shapes live in [`mesh_proto::facts`]. What is here is the half
//! that touches a machine: the built-in `system` collector, the JSON
//! canonicalization the digest is taken over, the flap guard, and the
//! redaction lint every document passes through before it can become
//! replicated state.
//!
//! ## The publish rule, in one sentence
//!
//! **A node authors an op only when the digest changes.** A stable machine
//! authors one op per collector, ever. Everything else here exists to make
//! that sentence true: canonical encoding so key order cannot fake a
//! change, the property-not-measurement rule so time cannot, and the flap
//! guard so a collector that breaks both is caught by the system rather
//! than by a person reading an op-log.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use mesh_proto::{FLAP_WINDOW_SECS, FactChange, FactSubject, MAX_FACT_DOC};

use crate::error::{CoreError, Result};

/// How often the collector loop wakes up. Individual collectors have their
/// own intervals; this is only the granularity at which they are noticed.
pub const TICK: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Canonical documents
// ---------------------------------------------------------------------------

/// Re-encode a JSON value canonically: object keys sorted, no whitespace.
///
/// The digest is over these bytes, so a collector that prints the same
/// facts in a different order twice produces the same digest and authors no
/// second op. Without this, "publish only when the digest changes" would be
/// a promise about a script's formatting rather than about a machine.
pub fn canonical(value: &serde_json::Value) -> String {
    fn write(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String((*key).clone()).to_string());
                    out.push(':');
                    if let Some(child) = map.get(*key) {
                        write(child, out);
                    }
                }
                out.push('}');
            }
            serde_json::Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// Flatten a document to `dotted.path -> scalar`, which is what a `fact.*`
/// selector resolves against and what a diff compares.
///
/// Arrays flatten by index (`disks.0.fstype`) **and** publish a joined form
/// at the array's own key, so `fact.groups~=wheel` works on a list without
/// anybody having to know it is one.
pub fn flatten(value: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(String::new(), value, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn walk(prefix: String, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(path, child, out);
            }
        }
        serde_json::Value::Array(items) => {
            let joined: Vec<String> = items.iter().map(scalar).collect();
            if !prefix.is_empty() {
                out.push((prefix.clone(), joined.join(",")));
            }
            for (i, item) in items.iter().enumerate() {
                walk(format!("{prefix}.{i}"), item, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.push((prefix, scalar(other)));
            }
        }
    }
}

fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Resolve a dotted path against a document, as a selector does.
pub fn lookup(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut cursor = value;
    for token in path.split('.') {
        cursor = match cursor {
            serde_json::Value::Object(map) => map.get(token)?,
            serde_json::Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    if let serde_json::Value::Array(items) = cursor {
        return Some(items.iter().map(scalar).collect::<Vec<String>>().join(","));
    }
    Some(scalar(cursor))
}

/// The document as a JSON value, or `Null` if it somehow is not JSON.
pub fn parse(doc: &str) -> serde_json::Value {
    serde_json::from_str(doc).unwrap_or(serde_json::Value::Null)
}

/// Every key that differs between two documents, in key order.
///
/// This is two features at once: `meshnode facts diff` (§4.8), and the flap
/// guard's diagnostic (§4.6) — the thing that turns "your collector is
/// wrong" into "your collector emits `uptime`".
pub fn changes(before: &serde_json::Value, after: &serde_json::Value) -> Vec<FactChange> {
    let old: BTreeMap<String, String> = flatten(before).into_iter().collect();
    let new: BTreeMap<String, String> = flatten(after).into_iter().collect();
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| old.get(*k) != new.get(*k))
        // An array's indexed children repeat what the joined form already
        // said. Reporting `packages` once beats reporting `packages.0`
        // through `packages.1841`.
        .filter(|k| {
            !k.rsplit_once('.').is_some_and(|(head, tail)| {
                tail.parse::<usize>().is_ok() && (new.contains_key(head) || old.contains_key(head))
            })
        })
        .map(|k| FactChange {
            key: k.clone(),
            before: old.get(k).cloned(),
            after: new.get(k).cloned(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Validation (§4.4, §9)
// ---------------------------------------------------------------------------

/// Turn a collector's stdout into a document ready to publish, or say
/// exactly what is wrong with it.
///
/// Every refusal names the thing that is wrong and what to do about it —
/// M10's validator set that standard and M18 is held to it.
///
/// `redactor` is the log spool's own [`crate::secrets::Redactor`], and the
/// lint is not optional: a collector that dumps `/proc/*/environ` is the
/// most obvious accident in this milestone, and replicated state is the
/// worst possible place for it to land (§9, R-4).
pub fn prepare(
    collector: &str,
    stdout: &str,
    redactor: Option<&crate::secrets::Redactor>,
) -> Result<(String, [u8; 32])> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Config(format!(
            "collector {collector:?} printed nothing — a collector is a script that \
             prints one JSON object on stdout"
        )));
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        CoreError::Config(format!(
            "collector {collector:?} did not print JSON ({e}) — it printed: {}",
            preview(trimmed)
        ))
    })?;
    if !value.is_object() {
        return Err(CoreError::Config(format!(
            "collector {collector:?} printed a {} — a fact document must be one JSON \
             object, so its keys can be selected on as fact.<key>",
            kind_of(&value)
        )));
    }
    let mut doc = canonical(&value);
    if let Some(redactor) = redactor {
        let cleaned = redactor.redact_text(&doc);
        if cleaned != doc {
            tracing::warn!(
                collector,
                "a fact document contained a known secret value; it was redacted \
                 before publication — fix the collector"
            );
            doc = cleaned;
        }
    }
    if doc.len() > MAX_FACT_DOC {
        return Err(CoreError::Config(format!(
            "collector {collector:?} produced {} bytes, over the {} byte cap — \
             there is nowhere for it to spill to on purpose: trim what it prints",
            doc.len(),
            MAX_FACT_DOC
        )));
    }
    let digest = *blake3::hash(doc.as_bytes()).as_bytes();
    Ok((doc, digest))
}

fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Array(_) => "JSON array",
        serde_json::Value::String(_) => "JSON string",
        serde_json::Value::Number(_) => "JSON number",
        serde_json::Value::Bool(_) => "JSON boolean",
        serde_json::Value::Null => "JSON null",
        serde_json::Value::Object(_) => "JSON object",
    }
}

fn preview(text: &str) -> String {
    let one_line: String = text.replace('\n', " ").chars().take(80).collect();
    format!("{one_line:?}")
}

// ---------------------------------------------------------------------------
// The flap guard (§4.6)
// ---------------------------------------------------------------------------

/// Per-collector publish bookkeeping for one node.
///
/// A node authors at most one publish per collector per
/// [`FLAP_WINDOW_SECS`], however much the document changes. **This is a
/// correctness feature, not polish**: without it, one badly written
/// collector on 1,000 nodes writes an unbounded, uncompactable op-log.
///
/// What makes it useful rather than merely safe is the diagnosis it keeps:
/// when a collector trips the limit, the keys that differed between the
/// last two documents are remembered, and `doctor` reports them.
#[derive(Debug, Default)]
pub struct FlapGuard {
    seen: Mutex<BTreeMap<String, Attempt>>,
}

#[derive(Debug, Clone)]
struct Attempt {
    /// The last document actually published, for the diff.
    published_doc: String,
    published_at_micros: u64,
    /// Set once the collector has been held back inside a window.
    flapping: Vec<FactChange>,
}

/// What [`FlapGuard::admit`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Publish it.
    Yes,
    /// The digest is unchanged. The common case, and the one the whole
    /// design is built around.
    Unchanged,
    /// Held back, with the keys that moved.
    Flapping(Vec<FactChange>),
}

impl FlapGuard {
    pub fn new() -> FlapGuard {
        FlapGuard::default()
    }

    /// Decide whether `doc` may be published for `collector` now.
    ///
    /// `key` identifies the (collector, subject) pair — a host collector
    /// running against six hundred switches gets six hundred independent
    /// budgets, because six hundred machines changing is six hundred facts
    /// and not one collector misbehaving.
    /// Decide, without recording anything as published — see
    /// [`published`](Self::published).
    ///
    /// The split matters. This guard's whole job is to remember *what this
    /// node last put in the op-log*, and an `Admit::Yes` is a decision, not
    /// an outcome: authoring the op can still fail, most ordinarily on a
    /// node that has collected its facts before its own `MemberAdd` has
    /// reached it. When admitting also recorded, that failure was
    /// permanent — the guard believed the document was published, so every
    /// later attempt with the same (correctly unchanged) document came back
    /// `Unchanged`, and a node that had simply started a few seconds early
    /// never described itself again. A fact is a property of a machine, so
    /// "unchanged" would stay true until somebody rebooted it.
    pub fn admit(&self, key: &str, doc: &str, now_micros: u64) -> Admit {
        let mut seen = self.seen.lock();
        let Some(attempt) = seen.get_mut(key) else {
            return Admit::Yes;
        };
        if attempt.published_doc == doc {
            return Admit::Unchanged;
        }
        let elapsed = now_micros.saturating_sub(attempt.published_at_micros);
        if elapsed < FLAP_WINDOW_SECS * 1_000_000 {
            let moved = changes(&parse(&attempt.published_doc), &parse(doc));
            attempt.flapping = moved.clone();
            return Admit::Flapping(moved);
        }
        Admit::Yes
    }

    /// Record that `doc` reached the op-log. Called only after the op is
    /// authored, so a publish that could not be authored leaves the guard
    /// where it was and the next tick tries again.
    pub fn published(&self, key: &str, doc: &str, now_micros: u64) {
        let mut seen = self.seen.lock();
        let attempt = seen.entry(key.to_string()).or_insert_with(|| Attempt {
            published_doc: String::new(),
            published_at_micros: now_micros,
            flapping: Vec::new(),
        });
        attempt.published_doc = doc.to_string();
        attempt.published_at_micros = now_micros;
        attempt.flapping.clear();
    }

    /// Collectors this node has held back, with the keys that moved.
    /// What `doctor` reports and the Director shows.
    pub fn flapping(&self) -> Vec<(String, Vec<FactChange>)> {
        self.seen
            .lock()
            .iter()
            .filter(|(_, a)| !a.flapping.is_empty())
            .map(|(k, a)| (k.clone(), a.flapping.clone()))
            .collect()
    }

    /// Forget a collector's history — called when its record changes, so an
    /// edited collector is not judged on what the old one printed.
    pub fn forget(&self, key: &str) {
        self.seen.lock().remove(key);
    }

    /// Forget every subject of one collector, which is what an edit to the
    /// collector record means.
    pub fn forget_prefix(&self, prefix: &str) {
        self.seen.lock().retain(|key, _| !key.starts_with(prefix));
    }
}

/// The bookkeeping key for one collector against one subject.
pub fn guard_key(collector: &str, subject: &FactSubject) -> String {
    format!("{collector}\u{1}{}", subject.label())
}

// ---------------------------------------------------------------------------
// Comparison (§4.7)
// ---------------------------------------------------------------------------

/// What a `fact.*` selector can ask of a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Substring, written `~=`.
    Contains,
    /// A regular expression, unanchored, written `fact.<key>~/<pattern>/`
    /// (Phase C, E7 — Nomad's `regexp`).
    Regex,
    /// Some collector's document has this key: `fact.<key>?` (Nomad's
    /// `is_set`).
    IsSet,
    /// No collector's document has this key: `!fact.<key>?` (Nomad's
    /// `is_not_set`). The one test a subject with no facts at all passes.
    IsNotSet,
}

impl FactOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactOp::Eq => "=",
            FactOp::Ne => "!=",
            FactOp::Lt => "<",
            FactOp::Le => "<=",
            FactOp::Gt => ">",
            FactOp::Ge => ">=",
            FactOp::Contains => "~=",
            FactOp::Regex => "~/",
            FactOp::IsSet | FactOp::IsNotSet => "?",
        }
    }
}

/// How the two sides of an ordering comparison are read (Phase C, E7).
///
/// Written as a suffix on the key — `fact.kernel_release:version>=6.1` — so
/// the value is never re-interpreted: `fact.kernel_release>=6.1` means
/// exactly what it meant before this existed, on every node, which is what a
/// selector inside a signed grant or policy has to be able to rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Comparison {
    /// Numbers as numbers when both sides are one, otherwise strings.
    #[default]
    Plain,
    /// `:version` — the **leading dotted numbers** of each side, compared
    /// number by number, a missing part reading as zero. Whatever follows
    /// the numbers is ignored, so `6.1.0-18-amd64` is `6.1.0` and `6.10`
    /// is above `6.9`. The rule a kernel release or a distribution version
    /// needs, and it is the whole rule: a side that does not start with a
    /// number matches nothing.
    Version,
    /// `:semver` — semantic versioning as `mesh_proto::version` reads it: a
    /// pre-release sorts below its release, so `0.7.3-rc.1` is below
    /// `0.7.3`. The rule a build of *our* software needs. A side that does
    /// not parse matches nothing.
    Semver,
}

impl Comparison {
    fn suffix(&self) -> &'static str {
        match self {
            Comparison::Plain => "",
            Comparison::Version => ":version",
            Comparison::Semver => ":semver",
        }
    }
}

/// The largest compiled pattern a selector may carry. A selector is
/// evaluated against every member on every placement, so a pattern that
/// compiles to megabytes is a cost paid by the whole mesh.
const MAX_PATTERN_BYTES: usize = 1 << 20;

/// One `fact.<key><op><value>` test, parsed.
///
/// **The default comparison is string-and-number aware, and nothing else.**
/// "Is `6.1.0-18-amd64` less than `6.1`" has no answer that is right for
/// every distribution, and a guess here silently patches the wrong machines
/// (§4.7). Phase C added version comparison as something a selector *asks
/// for by name* — `:version` or `:semver` — with each rule stated, rather
/// than as a change to what `<` already meant.
#[derive(Debug, Clone)]
pub struct FactTest {
    pub key: String,
    pub op: FactOp,
    /// The right-hand side; for [`FactOp::Regex`], the pattern.
    pub value: String,
    pub compare: Comparison,
    /// Compiled once at parse, for [`FactOp::Regex`].
    pattern: Option<regex::Regex>,
}

/// Two tests are the same test when they are written the same way; the
/// compiled pattern follows from the text.
impl PartialEq for FactTest {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.op == other.op
            && self.value == other.value
            && self.compare == other.compare
    }
}

impl Eq for FactTest {}

/// Written back in the grammar it was parsed from, prefix included.
impl std::fmt::Display for FactTest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = mesh_proto::FACT_SELECTOR_PREFIX;
        match self.op {
            FactOp::IsSet => write!(f, "{prefix}{}?", self.key),
            FactOp::IsNotSet => write!(f, "!{prefix}{}?", self.key),
            FactOp::Regex => write!(f, "{prefix}{}~/{}/", self.key, self.value),
            op => write!(
                f,
                "{prefix}{}{}{}{}",
                self.key,
                self.compare.suffix(),
                op.as_str(),
                self.value
            ),
        }
    }
}

/// Characters that mean an operator, and so cannot be part of a key.
fn has_operator(text: &str) -> bool {
    text.contains(['=', '<', '>', '~', '!'])
}

impl FactTest {
    /// Parse `fact.os_id=debian`, or the same without the prefix.
    ///
    /// Order matters: the set test and the pattern are recognised before any
    /// comparison — a pattern may contain `=` — and two-character operators
    /// are tried first, or `kernel<=6.1` would parse as `kernel` `<` `=6.1`.
    ///
    /// **Every form Phase C added degrades closed on an older node**, the
    /// property `Selector`'s own doc depends on: `fact.gpu?` and
    /// `!fact.gpu?` do not parse there at all, and `fact.os~/…/` or
    /// `fact.kernel:version>=6.1` parse as a comparison on a key no document
    /// has (`os~/…`, `kernel:version`) — so an older node matches no member
    /// rather than a different set of them.
    pub fn parse(text: &str) -> std::result::Result<FactTest, String> {
        let trimmed = text.trim();
        let (negated, rest) = match trimmed.strip_prefix('!') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, trimmed),
        };
        let body = rest
            .strip_prefix(mesh_proto::FACT_SELECTOR_PREFIX)
            .unwrap_or(rest);
        let test = |key: &str, op: FactOp, value: &str, compare: Comparison| FactTest {
            key: key.to_string(),
            op,
            value: value.to_string(),
            compare,
            pattern: None,
        };

        // `fact.gpu.vendor?` / `!fact.gpu.vendor?`
        if let Some(key) = body.strip_suffix('?')
            && !has_operator(key)
        {
            let key = key.trim();
            if key.is_empty() {
                return Err(format!(
                    "{text:?} has no fact key before `?` — try fact.gpu.vendor?"
                ));
            }
            let op = if negated {
                FactOp::IsNotSet
            } else {
                FactOp::IsSet
            };
            return Ok(test(key, op, "", Comparison::Plain));
        }
        if negated {
            return Err(format!(
                "{text:?}: only a set test can be negated — `!fact.<key>?` matches a node \
                 with no such fact. For a comparison, invert the operator instead (`!=`)"
            ));
        }

        // `fact.os.version~/^24\./`
        if let Some((key, rest)) = body.split_once("~/") {
            let key = key.trim();
            let Some(pattern) = rest.strip_suffix('/') else {
                return Err(format!(
                    "{text:?}: a pattern is written between two slashes — \
                     fact.<key>~/<pattern>/"
                ));
            };
            if key.is_empty() || has_operator(key) {
                return Err(format!(
                    "{text:?} has no fact key before `~/` — try fact.os.version~/^24\\./"
                ));
            }
            if pattern.is_empty() {
                return Err(format!(
                    "{text:?} has an empty pattern, which matches every value — \
                     use fact.{key}? to ask whether it is set"
                ));
            }
            let compiled = regex::RegexBuilder::new(pattern)
                .size_limit(MAX_PATTERN_BYTES)
                .build()
                .map_err(|e| format!("{text:?}: the pattern does not compile: {e}"))?;
            let mut t = test(key, FactOp::Regex, pattern, Comparison::Plain);
            t.pattern = Some(compiled);
            return Ok(t);
        }

        for (token, op) in [
            ("!=", FactOp::Ne),
            ("~=", FactOp::Contains),
            ("<=", FactOp::Le),
            (">=", FactOp::Ge),
            ("=", FactOp::Eq),
            ("<", FactOp::Lt),
            (">", FactOp::Gt),
        ] {
            if let Some((key, value)) = body.split_once(token) {
                let key = key.trim();
                let (key, compare) = if let Some(k) = key.strip_suffix(":version") {
                    (k.trim_end(), Comparison::Version)
                } else if let Some(k) = key.strip_suffix(":semver") {
                    (k.trim_end(), Comparison::Semver)
                } else {
                    (key, Comparison::Plain)
                };
                if key.is_empty() {
                    return Err(format!(
                        "{text:?} has no fact key before {token:?} — try \
                         fact.os_id=debian"
                    ));
                }
                if op == FactOp::Contains && compare != Comparison::Plain {
                    return Err(format!(
                        "{text:?}: `~=` is a substring test, and a version has no substrings \
                         to compare — drop {:?}",
                        compare.suffix()
                    ));
                }
                return Ok(test(key, op, value.trim(), compare));
            }
        }
        Err(format!(
            "{text:?} is not a fact test — write fact.<key><op><value>, where <op> is one \
             of = != < <= > >= ~= (substring); fact.<key>~/<pattern>/; fact.<key>? \
             (is set) or !fact.<key>? (is not set). Add :version or :semver to the key to \
             compare versions"
        ))
    }

    /// Does `value` satisfy this test?
    ///
    /// For a plain comparison, numbers compare as numbers when **both**
    /// sides parse as one, and otherwise as strings. `:version` and
    /// `:semver` say their rules on [`Comparison`]. A value being present at
    /// all is what a set test is about, so [`FactOp::IsSet`] answers `true`
    /// here and [`FactOp::IsNotSet`] `false`; [`FactTest::matches_values`] is
    /// the form that also knows about absence.
    pub fn matches(&self, value: &str) -> bool {
        use std::cmp::Ordering;
        match self.op {
            FactOp::Contains => return value.contains(&self.value),
            FactOp::Regex => return self.pattern.as_ref().is_some_and(|p| p.is_match(value)),
            FactOp::IsSet => return true,
            FactOp::IsNotSet => return false,
            _ => {}
        }
        let order: Option<Ordering> = match self.compare {
            Comparison::Plain => match (value.parse::<f64>(), self.value.parse::<f64>()) {
                (Ok(a), Ok(b)) => a.partial_cmp(&b),
                _ => Some(value.cmp(self.value.as_str())),
            },
            Comparison::Version => match (leading_numbers(value), leading_numbers(&self.value)) {
                (Some(a), Some(b)) => Some(compare_numbers(&a, &b)),
                _ => None,
            },
            Comparison::Semver => {
                let parses = |v: &str| mesh_proto::version::version_key(v).0.is_some();
                (parses(value) && parses(&self.value))
                    .then(|| mesh_proto::version::version_cmp(value, &self.value))
            }
        };
        let Some(order) = order else {
            // A NaN on either side, or a version that is not one, compares
            // false to everything — `!=` included. Fail closed.
            return false;
        };
        match self.op {
            FactOp::Eq => order.is_eq(),
            FactOp::Ne => order.is_ne(),
            FactOp::Lt => order.is_lt(),
            FactOp::Le => order.is_le(),
            FactOp::Gt => order.is_gt(),
            FactOp::Ge => order.is_ge(),
            FactOp::Contains | FactOp::Regex | FactOp::IsSet | FactOp::IsNotSet => {
                unreachable!("handled above")
            }
        }
    }

    /// Does a subject whose documents hold `values` for this key pass?
    ///
    /// One value per collector that has the key; none at all when no
    /// document does. The form every caller holding a whole subject uses,
    /// because "is not set" is a statement about the values that are *not*
    /// there, which [`FactTest::matches`] cannot see.
    pub fn matches_values(&self, mut values: impl Iterator<Item = String>) -> bool {
        match self.op {
            FactOp::IsSet => values.next().is_some(),
            FactOp::IsNotSet => values.next().is_none(),
            _ => values.any(|v| self.matches(&v)),
        }
    }
}

/// `6.1.0-18-amd64` → `[6, 1, 0]`; `v24.04` → `[24, 4]`; `beta` → `None`.
fn leading_numbers(text: &str) -> Option<Vec<u64>> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = Vec::new();
    for segment in text.split('.') {
        let digits: &str = &segment[..segment
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(segment.len())];
        let Ok(n) = digits.parse::<u64>() else {
            break;
        };
        parts.push(n);
        if digits.len() < segment.len() {
            break;
        }
    }
    (!parts.is_empty()).then_some(parts)
}

/// Number by number, a missing part reading as zero: `6.1` equals `6.1.0`.
fn compare_numbers(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    (0..len)
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .cmp(&b.get(i).copied().unwrap_or(0))
        })
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

// ---------------------------------------------------------------------------
// Collector validation (§9)
// ---------------------------------------------------------------------------

/// Refuse a collector that cannot work, while somebody is watching.
///
/// Every refusal names the thing that is wrong and what to do about it.
/// A collector that fails silently on a thousand nodes an hour after
/// publication is the failure mode this exists to prevent.
pub fn validate_collector(collector: &mesh_proto::FactCollector) -> Result<()> {
    let name = collector.name.trim();
    if name.is_empty() {
        return Err(CoreError::Config("a collector needs a name".into()));
    }
    if name == mesh_proto::SYSTEM_COLLECTOR {
        return Err(CoreError::Config(format!(
            "{:?} is the built-in collector every node runs in process — pick another \
             name, or the two would overwrite each other's facts",
            mesh_proto::SYSTEM_COLLECTOR
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CoreError::Config(format!(
            "collector name {name:?} may use letters, digits, `-` and `_` — it becomes \
             a key in `meshnode facts`, and a name with a dot would be ambiguous with \
             a fact path"
        )));
    }
    if collector.interval_secs != 0 && collector.interval_secs < 60 {
        // Not a refusal: `interval()` clamps it. But saying so beats an
        // operator discovering the clamp by watching the clock.
        tracing::warn!(
            collector = name,
            asked = collector.interval_secs,
            "collector interval raised to 60s — facts are properties, and a property \
             that needs a faster cadence than a minute is a measurement"
        );
    }
    if collector.signature.len() != 64 {
        return Err(CoreError::Config(
            "collector is not signed — it must be signed by the user publishing it".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Collector definitions (§4.5, §4.9)
// ---------------------------------------------------------------------------

/// A collector as somebody writes it.
///
/// `deny_unknown_fields`, like a schedule's source and for the same reason:
/// a typo in something that runs unattended on every node is a collector
/// that silently reports nothing, and "ignore what you don't recognise" is
/// indefensible there.
///
/// ```yaml
/// collector: gpu
/// on: tag=gpu
/// every: 1h
/// bash: |-
///   nvidia-smi --query-gpu=name,driver_version --format=csv,noheader \
///     | jq -Rn '{gpu_model: (input | split(", ")[0])}'
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorSource {
    /// The collector's name, and the key its facts are stored under.
    pub collector: String,
    /// Which nodes run it. An ordinary node selector; empty means `any`.
    #[serde(default)]
    pub on: Option<String>,
    /// How often, e.g. `15m`, `6h`. Minimum one minute.
    #[serde(default)]
    pub every: Option<String>,
    /// `node`, the default and the only kind left. (`host` described
    /// inventory hosts, and went with the host inventory.)
    #[serde(default)]
    pub subject: Option<String>,
    /// A shell command that prints one JSON object.
    #[serde(default)]
    pub command: Option<String>,
    /// An inline script body. The key **is** the runtime, exactly as a
    /// workflow step spells it.
    #[serde(default)]
    pub bash: Option<String>,
    #[serde(default)]
    pub python: Option<String>,
    /// `requirements.txt`, for the Python runtime.
    #[serde(default)]
    pub requirements: Option<String>,
    /// A registered service: `name` or `name@4`.
    #[serde(default)]
    pub service: Option<String>,
    /// Service inputs.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Per-run timeout, e.g. `30s`. Two minutes by default.
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub description: String,
}

/// Parse a collector definition. JSON when it starts with `{`, YAML
/// otherwise — the same rule every other definition in the product uses.
pub fn parse_collector(
    source: &str,
    created_by: mesh_proto::UserId,
) -> Result<mesh_proto::FactCollector> {
    let trimmed = source.trim_start();
    let parsed: CollectorSource = if trimmed.starts_with('{') {
        serde_json::from_str(source)
            .map_err(|e| CoreError::Config(format!("collector is not valid JSON: {e}")))?
    } else {
        serde_saphyr::from_str(source)
            .map_err(|e| CoreError::Config(format!("collector is not valid YAML: {e}")))?
    };
    build_collector(parsed, created_by)
}

/// Build a record from a parsed definition, applying every rule that can be
/// checked without the mesh.
pub fn build_collector(
    src: CollectorSource,
    created_by: mesh_proto::UserId,
) -> Result<mesh_proto::FactCollector> {
    let run = collector_action(&src)?;
    let interval = match src
        .every
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        Some(text) => humantime::parse_duration(text)
            .map(|d| d.as_secs())
            .map_err(|e| CoreError::Config(format!("`every: {text:?}` is not a duration: {e}")))?,
        None => mesh_proto::SYSTEM_INTERVAL_SECS,
    };
    let timeout = match src
        .timeout
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(text) => Some(
            humantime::parse_duration(text)
                .map(|d| d.as_secs())
                .map_err(|e| {
                    CoreError::Config(format!("`timeout: {text:?}` is not a duration: {e}"))
                })?,
        ),
        None => None,
    };
    let subject = match mesh_proto::FactSubjectKind::parse(src.subject.as_deref().unwrap_or("node"))
    {
        Some(mesh_proto::FactSubjectKind::Node) => mesh_proto::FactSubjectKind::Node,
        // Collectors that described inventory hosts went with the host
        // inventory; one published before the removal simply runs nowhere.
        Some(mesh_proto::FactSubjectKind::Host) => {
            return Err(CoreError::Config(
                "`subject: host` described hosts in the host inventory, which was removed; \
                 a collector describes the node it runs on"
                    .into(),
            ));
        }
        None => {
            return Err(CoreError::Config(format!(
                "`subject: {:?}` must be \"node\"",
                src.subject.unwrap_or_default()
            )));
        }
    };
    let collector = mesh_proto::FactCollector {
        name: src.collector.trim().to_string(),
        run,
        on: src
            .on
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| "any".into()),
        interval_secs: interval,
        subject,
        paused: src.paused,
        timeout_secs: timeout,
        description: src.description,
        created_by,
        created_at_micros: mesh_proto::now_micros(),
        // Filled by `sign_collector`; `validate_collector` refuses a record
        // that reaches the daemon without one.
        signature: vec![0u8; 64],
    };
    validate_collector(&collector)?;
    Ok(collector)
}

fn collector_action(src: &CollectorSource) -> Result<mesh_proto::StepAction> {
    let named: Vec<&str> = [
        src.command.as_ref().map(|_| "command"),
        src.bash.as_ref().map(|_| "bash"),
        src.python.as_ref().map(|_| "python"),
        src.service.as_ref().map(|_| "service"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match named.as_slice() {
        [] => {
            return Err(CoreError::Config(
                "a collector needs something to run: `command:`, `bash:`, `python:` or \
                 `service:` — it is a script that prints one JSON object"
                    .into(),
            ));
        }
        [_] => {}
        many => {
            return Err(CoreError::Config(format!(
                "a collector runs one thing, and this names {}: {}",
                many.len(),
                many.join(", ")
            )));
        }
    }

    if let Some(command) = &src.command {
        return Ok(mesh_proto::StepAction::Command(command.clone()));
    }
    if let Some(body) = &src.bash {
        return Ok(mesh_proto::StepAction::Script(Box::new(
            mesh_proto::InlineScript {
                runtime: mesh_proto::Runtime::Bash,
                source: body.clone(),
                requirements: String::new(),
                inputs: Vec::new(),
            },
        )));
    }
    if let Some(body) = &src.python {
        return Ok(mesh_proto::StepAction::Script(Box::new(
            mesh_proto::InlineScript {
                runtime: mesh_proto::Runtime::Python,
                source: body.clone(),
                requirements: src.requirements.clone().unwrap_or_default(),
                inputs: Vec::new(),
            },
        )));
    }
    let reference = src.service.clone().unwrap_or_default();
    let (name, version) = match reference.split_once('@') {
        Some((name, v)) => (
            name.to_string(),
            Some(v.parse::<u32>().map_err(|_| {
                CoreError::Config(format!("`service: {reference:?}` — {v:?} is not a version"))
            })?),
        ),
        None => (reference, None),
    };
    Ok(mesh_proto::StepAction::Service {
        name,
        version,
        inputs: src
            .set
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    })
}

/// Sign a collector with the publishing user's key. The daemon never holds
/// that key, and every node re-verifies at merge time.
pub fn sign_collector(
    user: &crate::identity::Keypair,
    mut collector: mesh_proto::FactCollector,
) -> mesh_proto::FactCollector {
    collector.created_by = user.user_id();
    collector.signature = Vec::new();
    collector.signature = user.sign(&collector.signing_bytes()).to_vec();
    collector
}

/// Render a collector back as the YAML that produced it, so `collectors ls`
/// and a Director drill-in can show the source of a record nobody kept a
/// file for.
pub fn to_yaml(collector: &mesh_proto::FactCollector) -> String {
    let mut out = String::new();
    out.push_str(&format!("collector: {}\n", collector.name));
    out.push_str(&format!("on: {}\n", collector.on));
    out.push_str(&format!("every: {}s\n", collector.interval()));
    if collector.subject != mesh_proto::FactSubjectKind::Node {
        out.push_str(&format!("subject: {}\n", collector.subject.as_str()));
    }
    if let Some(t) = collector.timeout_secs {
        out.push_str(&format!("timeout: {t}s\n"));
    }
    if collector.paused {
        out.push_str("paused: true\n");
    }
    if !collector.description.is_empty() {
        out.push_str(&format!("description: {:?}\n", collector.description));
    }
    match &collector.run {
        mesh_proto::StepAction::Command(cmd) => {
            out.push_str(&format!("command: {cmd:?}\n"));
        }
        mesh_proto::StepAction::Script(script) => {
            let key = match script.runtime {
                mesh_proto::Runtime::Python => "python",
                _ => "bash",
            };
            out.push_str(&format!("{key}: |-\n"));
            for line in script.source.lines() {
                out.push_str(&format!("  {line}\n"));
            }
            if !script.requirements.is_empty() {
                out.push_str(&format!("requirements: {:?}\n", script.requirements));
            }
        }
        mesh_proto::StepAction::Service {
            name,
            version,
            inputs,
        } => {
            match version {
                Some(v) => out.push_str(&format!("service: {name}@{v}\n")),
                None => out.push_str(&format!("service: {name}\n")),
            }
            if !inputs.is_empty() {
                out.push_str("set:\n");
                for (k, v) in inputs {
                    out.push_str(&format!("  {k}: {v:?}\n"));
                }
            }
        }
        other => out.push_str(&format!("# unrepresentable: {other:?}\n")),
    }
    out
}

// ---------------------------------------------------------------------------
// The built-in `system` collector (§4.5)
// ---------------------------------------------------------------------------

/// Read this machine and produce the `system` document.
///
/// Every probe is best-effort and a failed one is simply *absent* — never
/// zero, never `"unknown"`. An absent key matches no `fact.*` selector,
/// which is the fail-closed direction: a node that cannot say what kernel
/// it runs must not be patched as though it ran the right one.
///
/// **Nothing here is a measurement.** Free memory, load and uptime are
/// deliberately missing: they belong to M9 pressure, they change when
/// nothing changed, and publishing them would author an op every interval
/// on every node forever.
pub fn system_document(data_dir: &Path) -> serde_json::Value {
    let mut doc = serde_json::Map::new();
    let mut put = |key: &str, value: serde_json::Value| {
        if !value.is_null() {
            doc.insert(key.to_string(), value);
        }
    };

    // -- identity ---------------------------------------------------------
    put("hostname", str_or_null(hostname()));
    put("arch", json_str(std::env::consts::ARCH));
    put("os", json_str(std::env::consts::OS));
    put("meshware_version", json_str(env!("CARGO_PKG_VERSION")));

    // -- distribution -----------------------------------------------------
    let os_release = read_kv("/etc/os-release");
    put("os_id", str_or_null(os_release.get("ID").cloned()));
    put(
        "os_name",
        str_or_null(
            os_release
                .get("PRETTY_NAME")
                .or_else(|| os_release.get("NAME"))
                .cloned(),
        ),
    );
    put(
        "os_version",
        str_or_null(
            os_release
                .get("VERSION_ID")
                .or_else(|| os_release.get("VERSION"))
                .cloned(),
        ),
    );
    put("os_family", str_or_null(os_release.get("ID_LIKE").cloned()));

    // -- kernel -----------------------------------------------------------
    put(
        "kernel_release",
        str_or_null(proc_line("/proc/sys/kernel/osrelease")),
    );
    put(
        "kernel_version",
        str_or_null(proc_line("/proc/sys/kernel/version")),
    );

    // -- hardware ---------------------------------------------------------
    let cores = crate::capacity::cores();
    if cores > 0 {
        put("cpu_cores", serde_json::json!(cores));
    }
    put("cpu_model", str_or_null(cpu_model()));
    if let Some(mb) = crate::capacity::mem_total_mb() {
        put("memory_mb", serde_json::json!(mb));
    }
    if let Some(mb) = crate::capacity::disk_total_mb(data_dir) {
        put("disk_total_mb", serde_json::json!(mb));
    }
    // Deliberately *total*, never free: free disk is a measurement and
    // belongs to pressure (§4.2). Which filesystem the data dir sits on is
    // a property, and it is the one that decides where jobs land.
    put("data_dir_fstype", str_or_null(fstype_of(data_dir)));

    // -- virtualization ---------------------------------------------------
    put("virtualization", str_or_null(virtualization()));
    put("container", str_or_null(container()));

    // -- boot -------------------------------------------------------------
    // The boot id changes on reboot and *only* on reboot, which makes it a
    // property of this boot rather than a clock: `facts diff` showing it
    // move is how you learn a machine restarted without anybody saying so.
    put(
        "boot_id",
        str_or_null(proc_line("/proc/sys/kernel/random/boot_id")),
    );
    put("machine_id", str_or_null(proc_line("/etc/machine-id")));

    // -- package manager --------------------------------------------------
    put("package_manager", str_or_null(package_manager()));
    put("init_system", str_or_null(init_system()));

    serde_json::Value::Object(doc)
}

fn json_str(s: &str) -> serde_json::Value {
    serde_json::Value::String(s.to_string())
}

fn str_or_null(value: Option<String>) -> serde_json::Value {
    match value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(v) => serde_json::Value::String(v),
        None => serde_json::Value::Null,
    }
}

fn hostname() -> Option<String> {
    proc_line("/proc/sys/kernel/hostname").or_else(|| std::env::var("HOSTNAME").ok())
}

fn proc_line(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a `KEY=value` file, unquoting values the way `/etc/os-release`
/// writes them.
fn read_kv(path: &str) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

/// The CPU's marketing name, in the field each architecture actually uses.
///
/// Order matters and is the whole function. x86 publishes **both** a
/// numeric `model` (a stepping id) and a `model name`; matching the first
/// one that starts with `model` gets you `68` instead of
/// `AMD Ryzen 7 7735HS`, which is a fact that is technically true and
/// useless. So the keys are tried in priority order, and a value that is
/// nothing but digits is rejected — no architecture calls its processor
/// `68`.
fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let field = |want: &str| -> Option<String> {
        text.lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(want))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .filter(|v| !v.chars().all(|c| c.is_ascii_digit()))
    };
    // x86 and most ARM kernels; then ARM's board line, s390x, and ppc.
    field("model name")
        .or_else(|| field("Processor"))
        .or_else(|| field("Model"))
        .or_else(|| field("machine"))
        .or_else(|| field("cpu"))
}

/// The filesystem type backing `path`, read from `/proc/mounts` by longest
/// matching mount point.
fn fstype_of(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let _source = parts.next()?;
        let mount = parts.next()?;
        let fstype = parts.next()?;
        if target.starts_with(mount) {
            let depth = mount.len();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, fstype.to_string()));
            }
        }
    }
    best.map(|(_, fs)| fs)
}

/// What this machine is running *on*, best-effort and honestly absent when
/// nothing says. Read from DMI, which needs no privilege and no daemon.
fn virtualization() -> Option<String> {
    let vendor = proc_line("/sys/class/dmi/id/sys_vendor")
        .or_else(|| proc_line("/sys/class/dmi/id/product_name"))?;
    let lower = vendor.to_ascii_lowercase();
    for (needle, name) in [
        ("kvm", "kvm"),
        ("qemu", "qemu"),
        ("vmware", "vmware"),
        ("virtualbox", "virtualbox"),
        ("innotek", "virtualbox"),
        ("xen", "xen"),
        ("microsoft corporation", "hyper-v"),
        ("amazon", "aws"),
        ("google", "gce"),
        ("digitalocean", "digitalocean"),
        ("hetzner", "hetzner"),
        ("openstack", "openstack"),
        ("bochs", "qemu"),
    ] {
        if lower.contains(needle) {
            return Some(name.to_string());
        }
    }
    None
}

fn container() -> Option<String> {
    if Path::new("/.dockerenv").exists() {
        return Some("docker".into());
    }
    if Path::new("/run/.containerenv").exists() {
        return Some("podman".into());
    }
    std::env::var("container").ok().filter(|v| !v.is_empty())
}

fn package_manager() -> Option<String> {
    for (path, name) in [
        ("/usr/bin/apt-get", "apt"),
        ("/usr/bin/dnf", "dnf"),
        ("/usr/bin/yum", "yum"),
        ("/usr/bin/zypper", "zypper"),
        ("/usr/bin/pacman", "pacman"),
        ("/usr/bin/apk", "apk"),
        ("/usr/bin/emerge", "portage"),
        ("/run/current-system/sw/bin/nix-env", "nix"),
    ] {
        if Path::new(path).exists() {
            return Some(name.to_string());
        }
    }
    None
}

fn init_system() -> Option<String> {
    if Path::new("/run/systemd/system").exists() {
        return Some("systemd".into());
    }
    if Path::new("/sbin/openrc").exists() {
        return Some("openrc".into());
    }
    proc_line("/proc/1/comm")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_form_is_key_order_independent() {
        let a = json!({"b": 2, "a": 1, "c": {"z": 1, "y": 2}});
        let b = json!({"c": {"y": 2, "z": 1}, "a": 1, "b": 2});
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(canonical(&a), r#"{"a":1,"b":2,"c":{"y":2,"z":1}}"#);
    }

    #[test]
    fn flatten_publishes_a_list_both_ways() {
        let doc = json!({"groups": ["wheel", "docker"], "os": {"id": "debian"}});
        let flat: BTreeMap<String, String> = flatten(&doc).into_iter().collect();
        assert_eq!(flat["os.id"], "debian");
        assert_eq!(flat["groups"], "wheel,docker");
        assert_eq!(flat["groups.0"], "wheel");
    }

    #[test]
    fn lookup_walks_a_dotted_path() {
        let doc = json!({"os": {"id": "debian"}, "disks": [{"fstype": "ext4"}]});
        assert_eq!(lookup(&doc, "os.id").as_deref(), Some("debian"));
        assert_eq!(lookup(&doc, "disks.0.fstype").as_deref(), Some("ext4"));
        assert!(lookup(&doc, "os.nope").is_none());
    }

    #[test]
    fn changes_name_the_keys_that_differed() {
        let before = json!({"kernel": "6.1.0", "uptime": 100});
        let after = json!({"kernel": "6.1.0", "uptime": 160});
        let diff = changes(&before, &after);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].key, "uptime");
        assert_eq!(diff[0].verb(), "changed");
    }

    #[test]
    fn a_changed_list_reports_itself_once() {
        let before = json!({"packages": ["a", "b"]});
        let after = json!({"packages": ["a", "b", "c"]});
        let diff = changes(&before, &after);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert_eq!(diff[0].key, "packages");
    }

    // -- validation ---------------------------------------------------------

    #[test]
    fn a_collector_must_print_one_json_object() {
        let why = prepare("gpu", "", None).unwrap_err().to_string();
        assert!(why.contains("printed nothing"), "{why}");

        let why = prepare("gpu", "not json", None).unwrap_err().to_string();
        assert!(why.contains("did not print JSON"), "{why}");
        assert!(
            why.contains("not json"),
            "the message must show what it printed: {why}"
        );

        let why = prepare("gpu", "[1,2,3]", None).unwrap_err().to_string();
        assert!(why.contains("JSON array"), "{why}");
    }

    /// The cap refuses **on the node that produced it**, and names the byte
    /// count. There is no spill path on purpose.
    #[test]
    fn an_oversized_document_is_refused_and_names_the_bytes() {
        let big = json!({"blob": "x".repeat(MAX_FACT_DOC + 100)});
        let why = prepare("fat", &big.to_string(), None)
            .unwrap_err()
            .to_string();
        assert!(why.contains("over the"), "{why}");
        assert!(why.contains(&MAX_FACT_DOC.to_string()), "{why}");
    }

    #[test]
    fn a_prepared_document_is_canonical_and_hashed() {
        let (doc, digest) = prepare("x", r#"{"b":2,"a":1}"#, None).unwrap();
        assert_eq!(doc, r#"{"a":1,"b":2}"#);
        assert_eq!(digest, *blake3::hash(doc.as_bytes()).as_bytes());
        // Key order does not change the digest, which is what makes
        // "publish only on change" a statement about the machine.
        let (_, again) = prepare("x", r#"{"a":1,"b":2}"#, None).unwrap();
        assert_eq!(digest, again);
    }

    /// R-4: a collector that scrapes an environment must not put a known
    /// secret into replicated state.
    #[test]
    fn a_known_secret_is_redacted_before_publication() {
        let redactor = crate::secrets::Redactor::new(&[("tok".into(), b"SUPERSECRET".to_vec())])
            .expect("redactor");
        let (doc, _) = prepare("env", r#"{"token":"SUPERSECRET"}"#, Some(&redactor)).unwrap();
        assert!(!doc.contains("SUPERSECRET"), "{doc}");
    }

    // -- the flap guard -----------------------------------------------------

    #[test]
    fn an_unchanged_document_publishes_nothing() {
        let guard = FlapGuard::new();
        let doc = r#"{"kernel":"6.1.0"}"#;
        assert_eq!(guard.admit("system", doc, 0), Admit::Yes);
        guard.published("system", doc, 0);
        assert_eq!(guard.admit("system", doc, 60_000_000), Admit::Unchanged);
        assert_eq!(
            guard.admit("system", doc, FLAP_WINDOW_SECS * 2_000_000),
            Admit::Unchanged,
            "a stable machine authors one op per collector, ever"
        );
    }

    /// The regression behind an empty Facts panel that never filled in.
    ///
    /// `admit` says yes; authoring the op then fails, because this node
    /// collected its facts before its own `MemberAdd` reached it and the
    /// apply parks — and parking drops. If admitting had already recorded
    /// the document as published, every retry would answer `Unchanged`,
    /// and since a fact is a *property* the document would stay unchanged
    /// for the life of the machine: the node would never describe itself
    /// again, and no amount of waiting would fix it.
    #[test]
    fn a_publish_that_never_landed_is_admitted_again() {
        let guard = FlapGuard::new();
        let doc = r#"{"kernel":"6.1.0"}"#;
        assert_eq!(guard.admit("system", doc, 0), Admit::Yes);
        // author_op failed: nothing was recorded, so the next tick may try
        // the same document again — and is not accused of flapping for it.
        assert_eq!(guard.admit("system", doc, 30_000_000), Admit::Yes);
        guard.published("system", doc, 30_000_000);
        assert_eq!(guard.admit("system", doc, 60_000_000), Admit::Unchanged);
    }

    /// The exit demo: a collector that prints `uptime` is reported flapping
    /// **with `uptime` named**.
    #[test]
    fn a_flapping_collector_is_held_back_and_names_the_keys() {
        let guard = FlapGuard::new();
        assert_eq!(
            guard.admit("bad", r#"{"kernel":"6.1.0","uptime":100}"#, 0),
            Admit::Yes
        );
        guard.published("bad", r#"{"kernel":"6.1.0","uptime":100}"#, 0);
        let verdict = guard.admit("bad", r#"{"kernel":"6.1.0","uptime":160}"#, 60_000_000);
        let Admit::Flapping(keys) = verdict else {
            panic!("expected the guard to hold it back, got {verdict:?}");
        };
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "uptime");
        assert_eq!(guard.flapping().len(), 1);
        assert_eq!(guard.flapping()[0].0, "bad");
    }

    /// Past the window, a genuine change publishes again.
    #[test]
    fn a_real_change_publishes_once_the_window_passes() {
        let guard = FlapGuard::new();
        guard.published("system", r#"{"kernel":"6.1.0"}"#, 0);
        let later = FLAP_WINDOW_SECS * 1_000_000 + 1;
        assert_eq!(
            guard.admit("system", r#"{"kernel":"6.1.1"}"#, later),
            Admit::Yes
        );
        assert!(guard.flapping().is_empty(), "a real change is not flapping");
    }

    /// Each subject gets its own budget: six hundred switches changing is
    /// six hundred facts, not one collector misbehaving.
    #[test]
    fn subjects_have_independent_budgets() {
        let guard = FlapGuard::new();
        let a = guard_key("net", &FactSubject::Host("sw-1".into()));
        let b = guard_key("net", &FactSubject::Host("sw-2".into()));
        assert_eq!(guard.admit(&a, r#"{"x":1}"#, 0), Admit::Yes);
        guard.published(&a, r#"{"x":1}"#, 0);
        assert_eq!(guard.admit(&b, r#"{"x":2}"#, 0), Admit::Yes);
        guard.published(&b, r#"{"x":2}"#, 0);
    }

    // -- comparison ---------------------------------------------------------

    #[test]
    fn every_operator_parses_including_the_two_character_ones() {
        for (text, key, op, value) in [
            ("fact.os_id=debian", "os_id", FactOp::Eq, "debian"),
            ("fact.os_id!=debian", "os_id", FactOp::Ne, "debian"),
            ("fact.kernel<6.1", "kernel", FactOp::Lt, "6.1"),
            ("fact.kernel<=6.1", "kernel", FactOp::Le, "6.1"),
            ("fact.cores>8", "cores", FactOp::Gt, "8"),
            ("fact.cores>=8", "cores", FactOp::Ge, "8"),
            (
                "fact.gpu_model~=A100",
                "gpu_model",
                FactOp::Contains,
                "A100",
            ),
            // The prefix is optional: `facts query` takes the bare form.
            ("os_id=debian", "os_id", FactOp::Eq, "debian"),
        ] {
            let t = FactTest::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(
                (t.key.as_str(), t.op, t.value.as_str()),
                (key, op, value),
                "{text}"
            );
        }
    }

    #[test]
    fn numbers_compare_as_numbers_and_everything_else_as_strings() {
        assert!(FactTest::parse("fact.cores>=8").unwrap().matches("16"));
        // As strings, "16" < "8". As numbers, it is not.
        assert!(!FactTest::parse("fact.cores<8").unwrap().matches("16"));
        assert!(
            FactTest::parse("fact.os_id=debian")
                .unwrap()
                .matches("debian")
        );
        assert!(
            !FactTest::parse("fact.os_id=debian")
                .unwrap()
                .matches("ubuntu")
        );
        assert!(
            FactTest::parse("fact.kernel_release~=6.1.")
                .unwrap()
                .matches("6.1.0-18-amd64")
        );
    }

    /// A *plain* comparison still does not guess at versions. `6.1.0-18-amd64`
    /// is not a number, so it compares as a string — the same answer on
    /// every node, old and new, which is what a selector inside a signed
    /// grant relies on. Asking for a version is spelled `:version` (below).
    #[test]
    fn a_plain_comparison_does_not_guess_at_versions() {
        let t = FactTest::parse("fact.kernel_release<6.1").unwrap();
        assert_eq!(t.compare, Comparison::Plain);
        assert_eq!(t.matches("6.1.0-18-amd64"), "6.1.0-18-amd64" < "6.1");
    }

    /// Phase C, E7: `:version` reads the leading dotted numbers, which is
    /// what a kernel release needs — the Debian suffix is ignored, and 6.10
    /// is above 6.9, which neither a string nor a float comparison gets.
    #[test]
    fn a_version_comparison_reads_the_leading_numbers() {
        let at_least = FactTest::parse("fact.kernel_release:version>=6.1").unwrap();
        assert_eq!(at_least.key, "kernel_release");
        assert_eq!(at_least.compare, Comparison::Version);
        assert!(at_least.matches("6.1.0-18-amd64"));
        assert!(at_least.matches("6.10.2"));
        assert!(!at_least.matches("5.15.0-91-generic"));
        let below = FactTest::parse("fact.kernel_release:version<6.9").unwrap();
        assert!(!below.matches("6.10"), "6.10 is above 6.9");
        assert!(
            FactTest::parse("fact.os.version:version=24.04")
                .unwrap()
                .matches("24.4")
        );
        // Not a version at all: nothing matches, `!=` included.
        let ne = FactTest::parse("fact.kernel_release:version!=6.1").unwrap();
        assert!(!ne.matches("unknown"));
    }

    /// `:semver` is the other rule, for builds of our own software: a
    /// release candidate is below its release.
    #[test]
    fn a_semver_comparison_puts_a_pre_release_below_its_release() {
        let t = FactTest::parse("fact.agent:semver>=0.7.3").unwrap();
        assert!(t.matches("0.7.3"));
        assert!(t.matches("v0.8"));
        assert!(!t.matches("0.7.3-rc.1"));
        assert!(!t.matches("nightly"), "an unparseable side matches nothing");
        // Under `:version` the suffix is ignored, so the two rules disagree
        // on exactly this value — which is why both exist.
        assert!(
            FactTest::parse("fact.agent:version>=0.7.3")
                .unwrap()
                .matches("0.7.3-rc.1")
        );
    }

    #[test]
    fn a_pattern_is_unanchored_and_may_hold_operators() {
        let t = FactTest::parse(r"fact.os.version~/^24\./").unwrap();
        assert_eq!((t.key.as_str(), t.op), ("os.version", FactOp::Regex));
        assert!(t.matches("24.04"));
        assert!(!t.matches("22.04"));
        let t = FactTest::parse("fact.cmdline~/quiet=1|splash/").unwrap();
        assert!(t.matches("ro quiet=1"));
        assert_eq!(t.to_string(), "fact.cmdline~/quiet=1|splash/");
        let why = FactTest::parse("fact.os~/[unclosed/").unwrap_err();
        assert!(why.contains("does not compile"), "{why}");
        let why = FactTest::parse("fact.os~/debian").unwrap_err();
        assert!(why.contains("two slashes"), "{why}");
    }

    #[test]
    fn a_set_test_knows_about_absence() {
        let set = FactTest::parse("fact.gpu.vendor?").unwrap();
        let unset = FactTest::parse("!fact.gpu.vendor?").unwrap();
        assert_eq!((set.op, unset.op), (FactOp::IsSet, FactOp::IsNotSet));
        let none = || std::iter::empty::<String>();
        let one = || std::iter::once("nvidia".to_string());
        assert!(set.matches_values(one()) && !set.matches_values(none()));
        assert!(unset.matches_values(none()) && !unset.matches_values(one()));
        assert_eq!(unset.to_string(), "!fact.gpu.vendor?");
        let why = FactTest::parse("!fact.os_id=debian").unwrap_err();
        assert!(why.contains("only a set test"), "{why}");
    }

    /// Every form renders back to the text it was parsed from.
    #[test]
    fn every_form_round_trips() {
        for text in [
            "fact.os_id=debian",
            "fact.cores>=8",
            "fact.gpu_model~=A100",
            "fact.kernel_release:version>=6.1",
            "fact.agent:semver<0.8",
            r"fact.os.version~/^24\./",
            "fact.gpu.vendor?",
            "!fact.gpu.vendor?",
        ] {
            let t = FactTest::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(t.to_string(), text);
            assert_eq!(FactTest::parse(&t.to_string()).unwrap(), t);
        }
    }

    #[test]
    fn nonsense_says_what_a_comparison_looks_like() {
        let why = FactTest::parse("fact.os_id").unwrap_err();
        assert!(why.contains("<op>"), "{why}");
        let why = FactTest::parse("fact.=debian").unwrap_err();
        assert!(why.contains("no fact key"), "{why}");
        let why = FactTest::parse("fact.kernel:version~=6.1").unwrap_err();
        assert!(why.contains("substring"), "{why}");
    }

    // -- collector validation -----------------------------------------------

    fn collector(name: &str) -> mesh_proto::FactCollector {
        mesh_proto::FactCollector {
            name: name.into(),
            run: mesh_proto::StepAction::Command("echo {}".into()),
            on: "any".into(),
            interval_secs: 300,
            subject: mesh_proto::FactSubjectKind::Node,
            paused: false,
            timeout_secs: None,
            description: String::new(),
            created_by: mesh_proto::UserId([0u8; 32]),
            created_at_micros: 0,
            signature: vec![0u8; 64],
        }
    }

    #[test]
    fn the_built_in_name_is_reserved() {
        let why = validate_collector(&collector("system"))
            .unwrap_err()
            .to_string();
        assert!(why.contains("built-in"), "{why}");
    }

    #[test]
    fn a_dotted_name_is_refused_because_it_would_read_as_a_path() {
        let why = validate_collector(&collector("gpu.facts"))
            .unwrap_err()
            .to_string();
        assert!(why.contains("fact path"), "{why}");
    }

    #[test]
    fn an_unsigned_collector_is_refused() {
        let mut c = collector("gpu");
        c.signature.clear();
        assert!(validate_collector(&c).is_err());
        assert!(validate_collector(&collector("gpu")).is_ok());
    }

    // -- the built-in collector ---------------------------------------------

    /// The `system` document must be publishable on the machine running the
    /// tests, whatever that machine is — that is the whole promise of a
    /// built-in collector.
    #[test]
    fn the_system_collector_produces_a_publishable_document() {
        let dir = std::env::temp_dir();
        let doc = system_document(&dir);
        let (canonical_doc, _) =
            prepare(mesh_proto::SYSTEM_COLLECTOR, &doc.to_string(), None).expect("publishable");
        assert!(canonical_doc.len() <= MAX_FACT_DOC);
        let flat: BTreeMap<String, String> = flatten(&doc).into_iter().collect();
        assert_eq!(
            flat.get("arch").map(String::as_str),
            Some(std::env::consts::ARCH)
        );
        assert!(flat.contains_key("meshware_version"));
        // §4.2's rule, enforced on our own collector: nothing in here may
        // be a measurement.
        for banned in [
            "uptime",
            "load",
            "memory_free_mb",
            "disk_free_mb",
            "processes",
        ] {
            assert!(
                !flat.contains_key(banned),
                "`{banned}` is a measurement, not a fact — it would author an op every \
                 interval on every node forever"
            );
        }
    }

    /// x86 publishes a numeric `model` *and* a `model name`, and picking
    /// the wrong one yields `68` — technically true, useless, and exactly
    /// the kind of fact that survives review because it is not empty.
    #[test]
    fn the_cpu_model_is_a_name_and_not_a_stepping_id() {
        let Some(model) = cpu_model() else {
            return; // No /proc/cpuinfo — nothing to assert about.
        };
        assert!(
            !model.chars().all(|c| c.is_ascii_digit()),
            "cpu_model returned {model:?}, which is a stepping id and not a processor"
        );
    }

    /// Two reads of an unchanged machine produce the same digest. If this
    /// ever fails, the built-in collector has started reporting a
    /// measurement and the whole storage argument goes with it.
    #[test]
    fn the_system_collector_is_stable_across_reads() {
        let dir = std::env::temp_dir();
        let (_, first) = prepare("system", &system_document(&dir).to_string(), None).unwrap();
        let (_, second) = prepare("system", &system_document(&dir).to_string(), None).unwrap();
        assert_eq!(first, second);
    }
}
