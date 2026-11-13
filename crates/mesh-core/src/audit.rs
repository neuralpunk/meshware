//! The audit log (MASTERPLAN §7.4): every authenticated action becomes an
//! op in a grow-only replicated log, tamper-evident because op envelopes
//! are node-signed and entries carry the acting user's request signature.
//! Queryable from any node; retention/rotation is a Day-2 concern (M6).

//!
//! ## Never dropped, never flooded (SEC-6)
//!
//! Audit rows used to go through the general op-rate limiter, budgeted by
//! kind: 960 at once, 240 a minute after that, and a five-minute pause after
//! sixty refusals — and a refused row only logged a warning while the action
//! went ahead. Denials for unknown keys are audited, so about 1,100 junk
//! requests switched a node's audit trail off for five minutes, and a CI
//! pipeline submitting more than four jobs a second silently lost rows.
//!
//! Now the limiter does not apply to audit rows at all
//! (`state::UNLIMITED_KINDS`). What kept the log bounded moves here, and is
//! narrower: **repeated denials, and anything a key the mesh has never
//! admitted does, are folded** into one full row per key and action per
//! [`DENIAL_WINDOW_MICROS`] plus one summary row saying how many more there
//! were. Every key the mesh has never admitted shares one window per action,
//! so a stranger minting a fresh key per request buys two rows a minute.
//! What an admitted user does successfully is written every time.
//!
//! And an action worth auditing is refused while this node cannot write to
//! its log at all (`StateEngine::write_failure`), rather than carried out
//! with nobody able to account for it.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use mesh_proto::{AuditEntry, AuditId, OpBody, UserId};
use tracing::warn;

use crate::rbac;
use crate::state::{MeshState, StateEngine};

/// How long repeated denials of one key and action fold into one row.
pub const DENIAL_WINDOW_MICROS: u64 = 60 * 1_000_000;

/// Windows tracked at once. Past this, a further admitted key's denials
/// share the strangers' window for that action rather than growing the map.
const MAX_WINDOWS: usize = 4096;

/// Distinct unknown keys a summary names.
const SAMPLE: usize = 8;

/// The folding state (SEC-6). One per node, on its state engine.
#[derive(Debug, Default)]
pub struct Denials {
    windows: Mutex<HashMap<(Option<UserId>, String), Window>>,
}

#[derive(Debug)]
struct Window {
    opened_micros: u64,
    /// Rows folded since the full one.
    more: u64,
    /// Who the full row was about.
    first: UserId,
    /// `denied`, `ok`, …: the full row's outcome up to its first colon.
    outcome: String,
    /// Further distinct keys, for a strangers' window.
    sample: Vec<UserId>,
}

/// A row saying how many were folded.
struct Summary {
    user: UserId,
    action: String,
    outcome: String,
    more: u64,
    sample: Vec<UserId>,
    strangers: bool,
}

impl Denials {
    /// Note one foldable row. `true` when it opens a window and should be
    /// written in full; the summaries are for windows that closed.
    fn note(
        &self,
        admitted: Option<UserId>,
        user: UserId,
        action: &str,
        outcome: &str,
        now: u64,
    ) -> (bool, Vec<Summary>) {
        let mut windows = self.windows.lock();
        let closed = close(&mut windows, now, false);
        let mut key = (admitted, action.to_string());
        if admitted.is_some() && windows.len() >= MAX_WINDOWS && !windows.contains_key(&key) {
            key.0 = None;
        }
        match windows.get_mut(&key) {
            Some(window) => {
                window.more += 1;
                if key.0.is_none()
                    && window.first != user
                    && window.sample.len() < SAMPLE
                    && !window.sample.contains(&user)
                {
                    window.sample.push(user);
                }
                (false, closed)
            }
            None => {
                windows.insert(
                    key,
                    Window {
                        opened_micros: now,
                        more: 0,
                        first: user,
                        outcome: outcome.split(':').next().unwrap_or(outcome).to_string(),
                        sample: Vec::new(),
                    },
                );
                (true, closed)
            }
        }
    }

    /// Windows that have closed by `now` — or all of them, when `all`.
    fn closed(&self, now: u64, all: bool) -> Vec<Summary> {
        close(&mut self.windows.lock(), now, all)
    }
}

fn close(
    windows: &mut HashMap<(Option<UserId>, String), Window>,
    now: u64,
    all: bool,
) -> Vec<Summary> {
    let mut out = Vec::new();
    windows.retain(|(admitted, action), w| {
        if !all && now.saturating_sub(w.opened_micros) < DENIAL_WINDOW_MICROS {
            return true;
        }
        if w.more > 0 {
            out.push(Summary {
                user: w.first,
                action: action.clone(),
                outcome: w.outcome.clone(),
                more: w.more,
                sample: std::mem::take(&mut w.sample),
                strangers: admitted.is_none(),
            });
        }
        false
    });
    out
}

/// Append one audit event, authored by this node.
///
/// Failures are logged rather than propagated — the action this row
/// describes already happened or was already refused — and they mark the
/// engine as unable to write, which refuses further audit-worthy actions
/// until a write succeeds (SEC-6).
#[allow(clippy::too_many_arguments)]
pub fn record(
    engine: &Arc<StateEngine>,
    user: UserId,
    action: &str,
    target: &str,
    outcome: &str,
    detail: &str,
    request_sig: &[u8],
) {
    let founder = engine.genesis().body.founder_user;
    let (username, admitted) = engine.read(|s| {
        (
            rbac::username_of(s, &founder, &user),
            user.0 == founder || s.users.contains_key(&user),
        )
    });
    let now = mesh_proto::now_micros();
    if outcome.starts_with("denied") || !admitted {
        let (full, closed) =
            engine
                .audit_denials()
                .note(admitted.then_some(user), user, action, outcome, now);
        write_summaries(engine, &founder, closed);
        if !full {
            return;
        }
    }
    write(
        engine,
        AuditEntry {
            audit_id: AuditId::generate(),
            at_micros: now,
            user,
            username,
            action: action.to_string(),
            target: target.to_string(),
            node: engine.node_id(),
            outcome: outcome.to_string(),
            detail: detail.to_string(),
            request_sig: request_sig.to_vec(),
        },
    );
}

/// Write the summaries of folded windows that have closed — all of them
/// when `all` (a node shutting down, a test).
pub fn flush_folded(engine: &Arc<StateEngine>, all: bool) {
    let founder = engine.genesis().body.founder_user;
    let closed = engine.audit_denials().closed(mesh_proto::now_micros(), all);
    write_summaries(engine, &founder, closed);
}

fn write_summaries(engine: &Arc<StateEngine>, founder: &[u8; 32], closed: Vec<Summary>) {
    for summary in closed {
        let username = if summary.strangers {
            "(keys this mesh has not admitted)".to_string()
        } else {
            engine.read(|s| rbac::username_of(s, founder, &summary.user))
        };
        let window_secs = DENIAL_WINDOW_MICROS / 1_000_000;
        let detail = if summary.strangers {
            let keys: Vec<String> = std::iter::once(summary.user)
                .chain(summary.sample.iter().copied())
                .map(|k| k.short())
                .collect();
            format!(
                "{} more within {window_secs}s from keys this mesh has not admitted, \
                 {} distinct shown: {}",
                summary.more,
                keys.len(),
                keys.join(", ")
            )
        } else {
            format!("{} more within {window_secs}s", summary.more)
        };
        write(
            engine,
            AuditEntry {
                audit_id: AuditId::generate(),
                at_micros: mesh_proto::now_micros(),
                user: summary.user,
                username,
                action: summary.action,
                target: String::new(),
                node: engine.node_id(),
                outcome: format!("{} (×{} folded)", summary.outcome, summary.more),
                detail,
                request_sig: Vec::new(),
            },
        );
    }
}

fn write(engine: &Arc<StateEngine>, entry: AuditEntry) {
    let action = entry.action.clone();
    if let Err(e) = engine.author_op(OpBody::AuditAppend { entry }) {
        warn!(action, "failed to record audit entry: {e}");
    }
}

/// Query filters for `meshnode audit`.
#[derive(Debug, Default)]
pub struct AuditFilter {
    /// Username, or a `mwu1…` id (prefix match).
    pub user: Option<String>,
    /// Action prefix (`job.` matches `job.run` and `job.cancel`).
    pub action: Option<String>,
    pub since_micros: Option<u64>,
    pub limit: usize,
}

/// Does this entry pass the filter? The one definition, shared by the
/// in-memory tail and the history index (ENG-3).
pub fn matches(filter: &AuditFilter, e: &AuditEntry) -> bool {
    filter.since_micros.is_none_or(|t| e.at_micros >= t)
        && filter
            .user
            .as_deref()
            .is_none_or(|u| e.username == u || e.user.to_display_string().starts_with(u))
        && filter
            .action
            .as_deref()
            .is_none_or(|a| e.action.starts_with(a))
}

/// The page size a filter asks for.
pub fn limit_of(filter: &AuditFilter) -> usize {
    if filter.limit == 0 { 200 } else { filter.limit }
}

/// Filtered audit entries from the in-memory tail, newest first.
///
/// The tail is bounded (`state::AUDIT_RESIDENT`); a caller that needs more
/// continues in the history index from [`MeshState::audit`]'s oldest key.
pub fn query(state: &MeshState, filter: &AuditFilter) -> Vec<AuditEntry> {
    let limit = limit_of(filter);
    let mut out: Vec<AuditEntry> = state
        .audit
        .values()
        .rev() // AuditIds are time-prefixed: reverse = newest first
        .filter(|e| matches(filter, e))
        .take(limit)
        .cloned()
        .collect();
    // Stable order even when ids tie on the same millisecond.
    out.sort_by_key(|e| std::cmp::Reverse(e.audit_id));
    out
}
