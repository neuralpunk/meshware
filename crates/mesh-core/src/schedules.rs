//! Schedules: a service that runs itself (N1 P5).
//!
//! Cron was removed on 2026-09-12 for exactly one reason — it could fire
//! nothing but a workflow, and the workflow engine went. Everything else it
//! needed survived: rendezvous election, derived occurrence ids, catch-up,
//! overlap, and the trigger arithmetic in [`mesh_proto::schedule`]. This
//! module is that arithmetic reattached to the thing the product still has.
//!
//! Everything here is a pure function of a definition and replicated state.
//! The *firing* — election, dedup, dispatch — lives in the daemon; the
//! decisions live here, where they can be tested without a mesh.
//!
//! The one non-obvious rule is [`elected_firer`]. A schedule is owned by
//! whichever live node rendezvous-hashing picks for its **name**,
//! recomputed from each node's own view every tick — not by the node that
//! created it. A schedule outlives the laptop that made it, and a design
//! where the common case (that laptop is gone forever) is the exceptional
//! path is the wrong way round.

use std::collections::BTreeMap;
use std::sync::Arc;

use mesh_proto::{
    CatchUp, DEFAULT_GRACE_SECS, JobId, JobPriority, JobState, Liveness, MIN_INTERVAL_SECS, NodeId,
    OpBody, Overlap, ServiceSchedule, Trigger, UserId, is_valid_schedule_name,
    service_schedule_remove_signing_bytes,
};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::membership::Selector;
use crate::scheduler;
use crate::state::{MeshState, StateEngine};

/// Rendezvous context for firer election.
///
/// **Its own string, and deliberately not the retired workflow schedule's
/// `mw-schedule-v1`.** A node's rank for a service schedule should be
/// unrelated to its rank for anything else; two features whose owners
/// happened to coincide would look like a bug and would concentrate load
/// for real.
const ELECTION_CONTEXT: &[u8] = b"mw-service-schedule-v1";

/// How late an occurrence may be before it counts as *missed* rather than
/// merely noticed on the next tick. Generous relative to the tick so a busy
/// node, a gossip round or a slow election never turns a normal fire into a
/// catch-up decision.
pub const TICK_TOLERANCE_MICROS: u64 = 60 * 1_000_000;

// ---------------------------------------------------------------------------
// Definition source (TOML, YAML or JSON — the deployment convention)
// ---------------------------------------------------------------------------

/// A schedule as a person writes it.
///
/// `deny_unknown_fields` because a typo in an unattended trigger is a
/// schedule that silently does the wrong thing at 03:00 — the one place
/// where "ignore what you don't recognise" is indefensible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSource {
    #[serde(default)]
    pub schedule: ScheduleHeader,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleHeader {
    /// The schedule's name.
    pub name: String,
    /// `backup` or `backup@4`. A floating head is allowed: a nightly job
    /// is a thing that happens once and may be whatever the current
    /// version is when it does.
    pub service: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, toml::Value>,
    /// Where it may run, in the selector grammar. Defaults to `any`.
    #[serde(default)]
    pub targets: Option<String>,
    /// `15m`, `2h` — an interval, anchored on the epoch.
    #[serde(default)]
    pub every: Option<String>,
    /// `03:00` — a time of day.
    #[serde(default)]
    pub at: Option<String>,
    /// `mon-fri`, `sat,sun`, `*`. Only meaningful with `at`.
    #[serde(default)]
    pub days: Option<String>,
    /// `+02:00`, `-0500`, `UTC`. Fixed offsets only. Only with `at`.
    #[serde(default)]
    pub tz: Option<String>,
    /// `skip` (default) or `once`.
    #[serde(default, rename = "catch_up", alias = "catchUp")]
    pub catch_up: Option<String>,
    /// How late a `once` catch-up may still fire: `1h` by default.
    #[serde(default)]
    pub grace: Option<String>,
    /// `skip` (default) or `allow`.
    #[serde(default)]
    pub overlap: Option<String>,
    #[serde(default)]
    pub paused: bool,
    /// `low`, `normal` (default) or `high`.
    #[serde(default)]
    pub priority: Option<String>,
    /// Whole-job timeout, e.g. `45m`.
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// Parse a definition. The deployment heuristic verbatim, so one habit
/// covers both files.
pub fn parse(source: &str, created_by: UserId) -> Result<ServiceSchedule> {
    let trimmed = source.trim_start();
    let parsed: ScheduleSource = if trimmed.starts_with('{') {
        serde_json::from_str(source)
            .map_err(|e| CoreError::State(format!("schedule is not valid JSON: {e}")))?
    } else if looks_like_toml(trimmed) {
        toml::from_str(source)
            .map_err(|e| CoreError::State(format!("schedule is not valid TOML: {e}")))?
    } else {
        serde_saphyr::from_str(source)
            .map_err(|e| CoreError::State(format!("schedule is not valid YAML: {e}")))?
    };
    build(parsed, created_by)
}

/// The first real line decides, as it does for a deployment
/// (`deployments::looks_like_toml`): a table header, or `key = value`.
/// Testing for `[` only as the file's very first character read a TOML
/// schedule that opened with a comment — the way anybody documents one —
/// as YAML, and refused it at its own `[schedule]` header (found by DOC-6).
fn looks_like_toml(s: &str) -> bool {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .is_some_and(|l| l.starts_with('[') || (l.contains('=') && !l.trim_end().ends_with(':')))
}

/// Build a record from a parsed definition, applying every rule that can be
/// checked without the mesh.
pub fn build(src: ScheduleSource, created_by: UserId) -> Result<ServiceSchedule> {
    let h = src.schedule;
    if !is_valid_schedule_name(&h.name) {
        return Err(CoreError::State(format!(
            "schedule name {:?} must be 1-64 characters of letters, digits, '-', '_' or '.'",
            h.name
        )));
    }
    let (service, service_version) = split_service_ref(&h.service)?;
    let when = build_trigger(&h)?;
    let interval = when.min_interval_secs();
    if interval < MIN_INTERVAL_SECS {
        return Err(CoreError::State(format!(
            "a schedule may fire at most once every {MIN_INTERVAL_SECS}s (asked for {interval}s). \
             Cron's own resolution is a minute, and every fire costs a job record on every node \
             in the mesh"
        )));
    }
    let targets = h.targets.unwrap_or_else(|| "any".to_string());
    // Parsed once here so a malformed selector is a create-time error and
    // not a schedule that quietly matches nothing at 03:00.
    let _ = Selector::parse(&targets);

    let catch_up = match h.catch_up.as_deref().map(str::trim) {
        None | Some("") | Some("skip") | Some("none") => CatchUp::Skip,
        Some("once") => CatchUp::Once,
        Some(other) => {
            return Err(CoreError::State(format!(
                "catch_up must be \"skip\" or \"once\" (got {other:?}). There is deliberately no \
                 \"all\": a node down for a weekend would wake up and fire 192 backups"
            )));
        }
    };
    let overlap = match h.overlap.as_deref().map(str::trim) {
        None | Some("") | Some("skip") => Overlap::Skip,
        Some("allow") => Overlap::Allow,
        Some(other) => {
            return Err(CoreError::State(format!(
                "overlap must be \"skip\" or \"allow\" (got {other:?})"
            )));
        }
    };
    let priority = match h.priority.as_deref().map(str::trim) {
        None | Some("") | Some("normal") => JobPriority::Normal,
        Some("low") => JobPriority::Low,
        Some("high") => JobPriority::High,
        Some(other) => {
            return Err(CoreError::State(format!(
                "priority must be \"low\", \"normal\" or \"high\" (got {other:?})"
            )));
        }
    };
    let grace_secs = match h.grace.as_deref() {
        Some(text) => parse_duration_secs(text, "grace")?,
        None => DEFAULT_GRACE_SECS,
    };
    let timeout_secs = match h.timeout.as_deref() {
        Some(text) => Some(parse_duration_secs(text, "timeout")?),
        None => None,
    };
    let inputs = h
        .inputs
        .into_iter()
        .map(|(k, v)| (k, toml_scalar(&v)))
        .collect();

    Ok(ServiceSchedule {
        name: h.name,
        service,
        service_version,
        inputs,
        targets,
        when,
        catch_up,
        grace_secs,
        overlap,
        paused: h.paused,
        priority,
        timeout_secs,
        description: h.description,
        created_by,
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    })
}

fn toml_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn build_trigger(h: &ScheduleHeader) -> Result<Trigger> {
    match (h.every.as_deref(), h.at.as_deref()) {
        (Some(_), Some(_)) => Err(CoreError::State(
            "a schedule declares either `every` (an interval) or `at` (a time of day), not both"
                .into(),
        )),
        (None, None) => Err(CoreError::State(
            "a schedule needs `every = \"15m\"` or `at = \"03:00\"`".into(),
        )),
        (Some(every), None) => {
            if h.days.is_some() || h.tz.is_some() {
                return Err(CoreError::State(
                    "`days` and `tz` narrow a time of day, so they need `at`, not `every`".into(),
                ));
            }
            Ok(Trigger::Every {
                secs: parse_duration_secs(every, "every")?,
            })
        }
        (None, Some(at)) => {
            let minute_of_day = mesh_proto::schedule::parse_minute(at).ok_or_else(|| {
                CoreError::State(format!("`at = {at:?}` is not a time of day like \"03:00\""))
            })?;
            let days = match h.days.as_deref() {
                Some(text) => mesh_proto::schedule::parse_days(text).ok_or_else(|| {
                    CoreError::State(format!(
                        "`days = {text:?}` is not a day set like \"mon-fri\", \"sat,sun\" or \"*\""
                    ))
                })?,
                None => 0b0111_1111,
            };
            if days == 0 {
                return Err(CoreError::State(
                    "`days` selects no day, so this schedule could never fire".into(),
                ));
            }
            let utc_offset_minutes = match h.tz.as_deref() {
                Some(text) => mesh_proto::schedule::parse_offset(text).ok_or_else(|| {
                    CoreError::State(format!(
                        "`tz = {text:?}` is not a fixed UTC offset like \"+02:00\". A mesh carries \
                         no timezone database, deliberately: a DST transition that silently moves \
                         a production job by an hour is not worth the dependency"
                    ))
                })?,
                None => 0,
            };
            Ok(Trigger::Daily {
                minute_of_day,
                days,
                utc_offset_minutes,
            })
        }
    }
}

fn split_service_ref(text: &str) -> Result<(String, Option<u32>)> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CoreError::State("a schedule needs a `service`".into()));
    }
    match text.split_once('@') {
        None => Ok((text.to_string(), None)),
        Some((name, version)) => {
            let parsed = version.parse::<u32>().map_err(|_| {
                CoreError::State(format!(
                    "{text:?}: a pinned service version must be a number, as in \"backup@4\""
                ))
            })?;
            Ok((name.to_string(), Some(parsed)))
        }
    }
}

fn parse_duration_secs(text: &str, what: &str) -> Result<u64> {
    humantime::parse_duration(text.trim())
        .map(|d| d.as_secs())
        .map_err(|e| CoreError::State(format!("`{what} = {text:?}` is not a duration: {e}")))
}

// ---------------------------------------------------------------------------
// Validation against the mesh
// ---------------------------------------------------------------------------

/// A reason a schedule cannot be created, in the words the CLI prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub message: String,
}

/// Referential integrity, checked at **create** time on purpose: the point
/// of a schedule is that nobody is watching when it runs, so every error
/// that can surface while somebody is present must surface then.
pub fn validate(state: &MeshState, schedule: &ServiceSchedule) -> Vec<Issue> {
    let mut issues = Vec::new();
    match state.services.get(&schedule.service) {
        // Same wording as `deployments::validate`, and deliberately from
        // the same helper: the two refusals are the same fact about the
        // same mesh, and a reader who has met one should recognise the
        // other rather than wonder whether it means something else.
        None => issues.push(Issue {
            message: format!(
                "no service named {:?} is published. A schedule names a service; \
                 publish one first — Scripts → New service in Director, or \
                 `meshnode service create`{}",
                schedule.service,
                crate::deployments::published_hint(state)
            ),
        }),
        Some(entry) if entry.removed => issues.push(Issue {
            message: format!(
                "service {:?} has been retired. Publish it again, or point this \
                 schedule at one that is still live{}",
                schedule.service,
                crate::deployments::published_hint(state)
            ),
        }),
        Some(entry) => {
            if let Some(v) = schedule.service_version
                && !entry.versions.contains_key(&v)
            {
                let mut have: Vec<String> = entry.versions.keys().map(|n| n.to_string()).collect();
                have.sort_by_key(|n| n.parse::<u32>().unwrap_or(0));
                issues.push(Issue {
                    message: format!(
                        "service {} has no version {v}. Published: {}",
                        schedule.service,
                        if have.is_empty() {
                            "none yet".to_string()
                        } else {
                            have.join(", ")
                        }
                    ),
                });
            }
        }
    }
    issues
}

/// Every schedule naming `service`, so retiring one can say what it breaks.
pub fn service_is_scheduled(state: &MeshState, service: &str) -> Vec<String> {
    state
        .service_schedules
        .values()
        .filter(|e| !e.removed && e.schedule.service == service)
        .map(|e| e.schedule.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Signing and publication
// ---------------------------------------------------------------------------

pub fn sign(user: &Keypair, mut schedule: ServiceSchedule) -> ServiceSchedule {
    schedule.created_by = user.user_id();
    schedule.signature = Vec::new();
    schedule.signature = user.sign(&schedule.signing_bytes()).to_vec();
    schedule
}

pub fn sign_remove(user: &Keypair, name: &str) -> Vec<u8> {
    user.sign(&service_schedule_remove_signing_bytes(
        name,
        &user.user_id(),
    ))
    .to_vec()
}

pub fn put(engine: &Arc<StateEngine>, schedule: ServiceSchedule) -> Result<()> {
    engine.author_op(OpBody::ServiceSchedulePut {
        schedule: Box::new(schedule),
    })?;
    Ok(())
}

pub fn remove(engine: &Arc<StateEngine>, name: &str, by: UserId, signature: Vec<u8>) -> Result<()> {
    engine.author_op(OpBody::ServiceScheduleRemove {
        name: name.to_string(),
        by,
        signature,
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Election and authority
// ---------------------------------------------------------------------------

/// The node currently elected to fire `name`: rendezvous hashing over every
/// live member, recomputed from each node's own view every tick.
///
/// Disagreement during churn at worst fires an occurrence twice, and
/// [`JobId::derive_occurrence`] makes that converge to one job rather than
/// duplicate — the only guarantee actually available across a partition.
///
/// **Every live member, `no-execute` ones included.** Firing is
/// *coordination*, not execution: the firer authors a job, and the job is
/// then placed by the ordinary scheduler, which still refuses a
/// `no-execute` node as a candidate. Excluding the tag would mean a
/// Director-only install could never fire a schedule even while open, which
/// turns "I opted out of running work" into "I am not really a member".
pub fn elected_firer(
    state: &MeshState,
    name: &str,
    liveness_of: impl Fn(&NodeId) -> Liveness,
) -> Option<NodeId> {
    let pool: Vec<NodeId> = state
        .members
        .values()
        .filter(|m| state.is_active_member(&m.info.node_id))
        .filter(|m| matches!(liveness_of(&m.info.node_id), Liveness::Alive))
        .map(|m| m.info.node_id)
        .collect();
    scheduler::rendezvous_rank(ELECTION_CONTEXT, name.as_bytes(), pool)
        .into_iter()
        .next()
}

/// Is this schedule's creator still allowed to run what it fires?
///
/// Asked afresh on every tick and again on every executor, because that is
/// the whole point of pre-authorization by *record* rather than by stored
/// envelope: revoking the user, or taking away their role, stops the
/// schedule everywhere without recalling a signature or touching the
/// record. A stored envelope could not be withdrawn at all.
pub fn creator_may_still_run(
    state: &MeshState,
    founder: &[u8; 32],
    schedule: &ServiceSchedule,
    at_micros: u64,
) -> bool {
    crate::rbac::is_user_active(state, founder, &schedule.created_by)
        && crate::rbac::has_role(
            state,
            founder,
            &schedule.created_by,
            crate::rbac::SCHEDULES,
            at_micros,
        )
}

// ---------------------------------------------------------------------------
// The firing decision
// ---------------------------------------------------------------------------

/// Why an occurrence was not fired.
///
/// Every one of these is audited and shown by `schedule show`: a schedule
/// that has quietly done nothing for three weeks is this feature's likeliest
/// failure, and the only defence is that not-firing is as visible as firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// The previous job has not finished and `overlap = "skip"`.
    Overlap(JobId),
    /// A missed occurrence older than `grace`, or `catch_up = "skip"`.
    TooLate { occurrence_micros: u64 },
}

impl Skip {
    pub fn reason(&self) -> String {
        match self {
            Skip::Overlap(job) => format!("previous job {} is still running", job.short()),
            Skip::TooLate { .. } => "the occurrence is older than the catch-up grace".to_string(),
        }
    }
}

/// What a tick should do about one schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    Idle,
    Fire { occurrence_micros: u64 },
    Skipped { occurrence_micros: u64, skip: Skip },
}

/// Decide what to do about one schedule, right now.
///
/// A pure function of the schedule, the clock and what replicated state
/// says has already happened — which is what makes the whole firing rule
/// testable without a mesh, and what keeps two nodes from reaching
/// different answers for reasons neither can see.
///
/// `already_fired` answers "does a job for this occurrence exist?", which
/// is the dedup test: a job's id is derived from `(schedule, occurrence)`,
/// so the job **is** the record that the occurrence fired.
pub fn due(
    schedule: &ServiceSchedule,
    now_micros: u64,
    last_seen_micros: Option<u64>,
    already_fired: impl Fn(JobId) -> bool,
    running: impl Fn() -> Option<JobId>,
) -> Due {
    let Some(occurrence) = schedule.when.previous_at_or_before(now_micros) else {
        return Due::Idle;
    };
    // Never fire an occurrence from before the schedule existed: creating
    // one at 04:00 must not immediately run last night's 03:00.
    let floor = last_seen_micros.unwrap_or(schedule.created_at_micros);
    if occurrence < floor {
        return Due::Idle;
    }
    if already_fired(JobId::derive_occurrence(&schedule.name, occurrence)) {
        return Due::Idle;
    }

    // How late are we? On a healthy mesh this is a few seconds — the tick
    // interval. After a restart it can be hours, and that is the difference
    // between firing and catching up.
    let lateness = now_micros.saturating_sub(occurrence);
    let grace = schedule.grace_secs.saturating_mul(1_000_000);
    let missed = lateness > TICK_TOLERANCE_MICROS;
    if missed {
        match schedule.catch_up {
            CatchUp::Skip => {
                return Due::Skipped {
                    occurrence_micros: occurrence,
                    skip: Skip::TooLate {
                        occurrence_micros: occurrence,
                    },
                };
            }
            CatchUp::Once if lateness > grace => {
                return Due::Skipped {
                    occurrence_micros: occurrence,
                    skip: Skip::TooLate {
                        occurrence_micros: occurrence,
                    },
                };
            }
            CatchUp::Once => {}
        }
    }

    if schedule.overlap == Overlap::Skip
        && let Some(job) = running()
    {
        return Due::Skipped {
            occurrence_micros: occurrence,
            skip: Skip::Overlap(job),
        };
    }

    Due::Fire {
        occurrence_micros: occurrence,
    }
}

/// The most recent occurrence of `schedule` that actually produced a job,
/// scanning back over the last `look_back` occurrences.
///
/// Bounded on purpose: the alternative is a scan of every job in state per
/// schedule per tick, which on a mesh with a 1-minute schedule and a month
/// of history is forty thousand lookups to answer "when did this last
/// run".
pub fn last_fired(
    state: &MeshState,
    schedule: &ServiceSchedule,
    now_micros: u64,
    look_back: u32,
) -> Option<(u64, JobId)> {
    let mut at = schedule.when.previous_at_or_before(now_micros)?;
    for _ in 0..look_back {
        let id = JobId::derive_occurrence(&schedule.name, at);
        if state.jobs.contains_key(&id) {
            return Some((at, id));
        }
        at = schedule.when.previous_at_or_before(at.checked_sub(1)?)?;
    }
    None
}

/// A job this schedule fired that has not finished, if there is one.
pub fn running_job(
    state: &MeshState,
    schedule: &ServiceSchedule,
    now_micros: u64,
    look_back: u32,
) -> Option<JobId> {
    let mut at = schedule.when.previous_at_or_before(now_micros)?;
    for _ in 0..look_back {
        let id = JobId::derive_occurrence(&schedule.name, at);
        if let Some(job) = state.jobs.get(&id)
            && !job.state.is_terminal()
            && matches!(job.state, JobState::Running | JobState::Queued)
        {
            return Some(id);
        }
        at = schedule.when.previous_at_or_before(at.checked_sub(1)?)?;
    }
    None
}

/// How many past occurrences `last_fired` and `running_job` look back over.
///
/// Twenty is generous for "did the last one finish" and bounded enough that
/// a one-minute schedule costs twenty map lookups a tick rather than a scan.
pub const LOOK_BACK: u32 = 20;

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a record back to the TOML a person would edit, so
/// `schedule show --output toml` round-trips into `schedule put`.
pub fn to_toml(s: &ServiceSchedule) -> String {
    let mut out = String::new();
    out.push_str("[schedule]\n");
    out.push_str(&format!("name    = {:?}\n", s.name));
    out.push_str(&format!("service = {:?}\n", s.service_ref()));
    out.push_str(&format!("targets = {:?}\n", s.targets));
    match s.when {
        Trigger::Every { secs } => out.push_str(&format!("every   = \"{secs}s\"\n")),
        Trigger::Daily {
            minute_of_day,
            days,
            utc_offset_minutes,
        } => {
            out.push_str(&format!(
                "at      = \"{:02}:{:02}\"\n",
                minute_of_day / 60,
                minute_of_day % 60
            ));
            out.push_str(&format!(
                "days    = {:?}\n",
                mesh_proto::schedule::render_days(days)
            ));
            out.push_str(&format!(
                "tz      = {:?}\n",
                mesh_proto::schedule::render_offset(utc_offset_minutes)
            ));
        }
    }
    out.push_str(&format!("catch_up = {:?}\n", s.catch_up.as_str()));
    out.push_str(&format!("grace    = \"{}s\"\n", s.grace_secs));
    out.push_str(&format!("overlap  = {:?}\n", s.overlap.as_str()));
    out.push_str(&format!("priority = {:?}\n", priority_str(s.priority)));
    if let Some(t) = s.timeout_secs {
        out.push_str(&format!("timeout  = \"{t}s\"\n"));
    }
    if s.paused {
        out.push_str("paused   = true\n");
    }
    if !s.description.is_empty() {
        out.push_str(&format!("description = {:?}\n", s.description));
    }
    if !s.inputs.is_empty() {
        out.push_str("\n[schedule.inputs]\n");
        for (k, v) in &s.inputs {
            out.push_str(&format!("{k} = {v:?}\n"));
        }
    }
    out
}

fn priority_str(p: JobPriority) -> &'static str {
    match p {
        JobPriority::Low => "low",
        JobPriority::Normal => "normal",
        JobPriority::High => "high",
    }
}

// ---------------------------------------------------------------------------
// The firing rules, tested without a mesh
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-01-05T00:00:00Z was a Monday — the same anchor
    /// `mesh_proto::schedule`'s own tests use, for the same reason: a
    /// weekday bug that only appears in one timezone is exactly what this
    /// arithmetic exists to avoid.
    const MON_MIDNIGHT: u64 = 1_767_571_200_000_000;
    const MINUTE: u64 = 60_000_000;
    const HOUR: u64 = 3_600_000_000;

    fn schedule(src: &str) -> ServiceSchedule {
        parse(src, UserId([0u8; 32])).expect("parse")
    }

    fn nightly() -> ServiceSchedule {
        let mut s = schedule(
            r#"
[schedule]
name    = "backup"
service = "backup"
at      = "03:00"
"#,
        );
        s.created_at_micros = MON_MIDNIGHT;
        s
    }

    #[test]
    fn a_toml_file_that_opens_with_a_comment_is_still_toml() {
        // The way the documentation writes one, and the way anybody would:
        // a comment naming the file, then the table.
        let s = schedule(
            "# nightly-backup.toml\n[schedule]\nname = \"nightly\"\nservice = \"backup\"\nat = \"03:00\"\n",
        );
        assert_eq!(s.name, "nightly");
    }

    #[test]
    fn a_definition_parses_into_the_trigger_it_names() {
        let s = schedule(
            r#"
[schedule]
name     = "nightly"
service  = "backup@4"
targets  = "tag=storage"
at       = "03:00"
days     = "mon-fri"
tz       = "+02:00"
catch_up = "once"
grace    = "2h"
overlap  = "allow"
priority = "low"
timeout  = "45m"
"#,
        );
        assert_eq!(s.service, "backup");
        assert_eq!(s.service_version, Some(4));
        assert_eq!(s.targets, "tag=storage");
        assert_eq!(s.catch_up, CatchUp::Once);
        assert_eq!(s.grace_secs, 7200);
        assert_eq!(s.overlap, Overlap::Allow);
        assert_eq!(s.timeout_secs, Some(2700));
        assert!(matches!(
            s.when,
            Trigger::Daily {
                minute_of_day: 180,
                days: 0b0001_1111,
                utc_offset_minutes: 120,
            }
        ));
    }

    /// The record renders back to a file its own parser accepts — the
    /// property `schedule show --output toml | schedule put` depends on.
    #[test]
    fn a_schedule_round_trips_through_its_own_toml() {
        for src in [
            r#"
[schedule]
name    = "a"
service = "backup@4"
every   = "15m"
"#,
            r#"
[schedule]
name     = "b"
service  = "report"
at       = "03:00"
days     = "sat,sun"
tz       = "-05:00"
catch_up = "once"

[schedule.inputs]
target = "s3://bucket"
"#,
        ] {
            let first = schedule(src);
            let round = parse(&to_toml(&first), UserId([0u8; 32])).expect("re-parse");
            assert_eq!(round.when, first.when, "trigger for {}", first.name);
            assert_eq!(round.service_ref(), first.service_ref());
            assert_eq!(round.catch_up, first.catch_up);
            assert_eq!(round.overlap, first.overlap);
            assert_eq!(round.inputs, first.inputs);
        }
    }

    /// Cron's own resolution is a minute, and every fire costs a job record
    /// on every node. A ten-second schedule is refused where the refusal is
    /// cheap: at create time.
    #[test]
    fn a_sub_minute_schedule_is_refused_at_create_time() {
        let err = parse(
            r#"
[schedule]
name    = "hot"
service = "poll"
every   = "10s"
"#,
            UserId([0u8; 32]),
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("at most once every"), "{err}");
    }

    /// A file with no trigger, or two, is a typo rather than a
    /// configuration — and an unattended typo is the one this module exists
    /// to catch while somebody is still present.
    #[test]
    fn a_schedule_with_no_trigger_or_two_is_refused() {
        for (src, want) in [
            (
                r#"
[schedule]
name    = "x"
service = "s"
"#,
                "needs `every",
            ),
            (
                r#"
[schedule]
name    = "x"
service = "s"
every   = "10m"
at      = "03:00"
"#,
                "not both",
            ),
            (
                r#"
[schedule]
name    = "x"
service = "s"
every   = "10m"
days    = "mon-fri"
"#,
                "need `at`",
            ),
        ] {
            let err = parse(src, UserId([0u8; 32])).expect_err("must refuse");
            assert!(err.to_string().contains(want), "wanted {want:?}, got {err}");
        }
    }

    /// Creating a schedule at 04:00 must not immediately run last night's
    /// 03:00. The floor is the record's own creation instant.
    #[test]
    fn a_new_schedule_does_not_fire_the_occurrence_before_it_existed() {
        let mut s = nightly();
        // Created at 04:00 Monday; 03:00 Monday is behind it.
        s.created_at_micros = MON_MIDNIGHT + 4 * HOUR;
        let at_0410 = MON_MIDNIGHT + 4 * HOUR + 10 * MINUTE;
        assert_eq!(due(&s, at_0410, None, |_| false, || None), Due::Idle);
    }

    /// The ordinary case: the occurrence has come round, nothing has run
    /// it, and we are the first to notice.
    #[test]
    fn an_occurrence_a_few_seconds_late_simply_fires() {
        let s = nightly();
        let just_after = MON_MIDNIGHT + 3 * HOUR + 5_000_000;
        assert_eq!(
            due(&s, just_after, None, |_| false, || None),
            Due::Fire {
                occurrence_micros: MON_MIDNIGHT + 3 * HOUR
            }
        );
    }

    /// The dedup rule, and the whole of the exactly-once story: a job whose
    /// id is derived from `(schedule, occurrence)` **is** the record that
    /// the occurrence fired, so a second firer finds it and stands down.
    #[test]
    fn an_occurrence_that_already_has_a_job_does_not_fire_again() {
        let s = nightly();
        let occurrence = MON_MIDNIGHT + 3 * HOUR;
        let existing = JobId::derive_occurrence("backup", occurrence);
        assert_eq!(
            due(
                &s,
                occurrence + 5_000_000,
                None,
                |id| id == existing,
                || None
            ),
            Due::Idle
        );
    }

    /// Two nodes that both believe they are the firer derive the *same* job
    /// id, which is what makes a double fire converge rather than run the
    /// backup twice.
    #[test]
    fn two_firers_derive_the_same_job_id() {
        let occurrence = MON_MIDNIGHT + 3 * HOUR;
        assert_eq!(
            JobId::derive_occurrence("backup", occurrence),
            JobId::derive_occurrence("backup", occurrence)
        );
        assert_ne!(
            JobId::derive_occurrence("backup", occurrence),
            JobId::derive_occurrence("backup", occurrence + MINUTE)
        );
        assert_ne!(
            JobId::derive_occurrence("backup", occurrence),
            JobId::derive_occurrence("report", occurrence)
        );
    }

    /// A node down for a weekend wakes up and does **not** fire 48 backups.
    /// `catch_up` decides between one and none; there is deliberately no
    /// "all".
    #[test]
    fn a_long_outage_fires_at_most_one_catch_up() {
        let occurrence = MON_MIDNIGHT + 3 * HOUR;
        let two_days_later = occurrence + 2 * 24 * HOUR;

        let mut skip = nightly();
        skip.catch_up = CatchUp::Skip;
        // The most recent occurrence is Wednesday 03:00, hours ago.
        assert!(matches!(
            due(&skip, two_days_later + 6 * HOUR, None, |_| false, || None),
            Due::Skipped { .. }
        ));

        let mut once = nightly();
        once.catch_up = CatchUp::Once;
        once.grace_secs = 3600;
        // Inside the grace: fire it, once.
        assert!(matches!(
            due(
                &once,
                two_days_later + 30 * MINUTE,
                None,
                |_| false,
                || None
            ),
            Due::Fire { .. }
        ));
        // Past the grace: let it go.
        assert!(matches!(
            due(&once, two_days_later + 6 * HOUR, None, |_| false, || None),
            Due::Skipped { .. }
        ));
    }

    /// `overlap = "skip"` is the default, and it is what stops a backup
    /// that takes 70 minutes from being run every hour forever.
    #[test]
    fn a_still_running_job_skips_the_next_occurrence_unless_overlap_is_allowed() {
        let mut hourly = schedule(
            r#"
[schedule]
name    = "backup"
service = "backup"
every   = "1h"
"#,
        );
        hourly.created_at_micros = MON_MIDNIGHT;
        let previous = JobId::derive_occurrence("backup", MON_MIDNIGHT + HOUR);
        let now = MON_MIDNIGHT + 2 * HOUR + 5_000_000;

        assert_eq!(
            due(&hourly, now, None, |_| false, || Some(previous)),
            Due::Skipped {
                occurrence_micros: MON_MIDNIGHT + 2 * HOUR,
                skip: Skip::Overlap(previous),
            }
        );

        hourly.overlap = Overlap::Allow;
        assert!(matches!(
            due(&hourly, now, None, |_| false, || Some(previous)),
            Due::Fire { .. }
        ));
    }

    /// A signed schedule authorizes the instants its own expression names,
    /// and no others — the anti-replay check the executor applies at every
    /// fire.
    #[test]
    fn a_schedule_names_only_its_own_occurrences() {
        let s = nightly();
        assert!(s.names_occurrence(MON_MIDNIGHT + 3 * HOUR));
        assert!(s.names_occurrence(MON_MIDNIGHT + 24 * HOUR + 3 * HOUR));
        assert!(!s.names_occurrence(MON_MIDNIGHT + 3 * HOUR + MINUTE));
        assert!(!s.names_occurrence(MON_MIDNIGHT + 4 * HOUR));
    }
}
