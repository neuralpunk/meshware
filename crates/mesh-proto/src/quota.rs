//! Fair share and preemption (C1 P4/P5) — and quotas' records, **retired**.
//!
//! What is live here is small: [`PreemptMode`], what reclaiming a slot does
//! to preemptible work, and [`QueueRow`], one line of `meshnode queue`.
//!
//! Quotas were removed (2026-09-18): ceilings, weighted shares, borrowing,
//! and the per-node reservation records a ceiling was checked against. The
//! shapes below are the payloads of `OpBody::QuotaPut` and
//! `OpBody::ReservationsPut`, kept so a log written before the removal
//! still decodes — postcard is positional. **Change nothing in them.**

use serde::{Deserialize, Serialize};

use crate::UserId;

/// What reclaiming a slot does to preemptible work. Cancel and requeue
/// only: suspend needs checkpoint/restore and is **not shipped rather than
/// faked**, because a `suspend` that means `SIGSTOP` holds the memory it
/// was meant to release (§5.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreemptMode {
    #[default]
    Off,
    Cancel,
    Requeue,
}

impl PreemptMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PreemptMode::Off => "off",
            PreemptMode::Cancel => "cancel",
            PreemptMode::Requeue => "requeue",
        }
    }

    pub fn parse(s: &str) -> Option<PreemptMode> {
        match s {
            "off" | "none" => Some(PreemptMode::Off),
            "cancel" => Some(PreemptMode::Cancel),
            "requeue" => Some(PreemptMode::Requeue),
            _ => None,
        }
    }
}

/// One waiting job, as `meshnode queue` shows it (C1 P4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRow {
    /// 1-based position in the order this node will actually run them.
    pub position: u32,
    pub job: crate::JobId,
    pub principal: UserId,
    pub username: String,
    pub priority: crate::JobPriority,
    /// The weighted dominant share this position was decided by. Lower is
    /// further behind, and further behind runs first.
    pub share_permille: u32,
}

// -- retired: quotas and reservations ------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCeiling {
    pub cpus: u32,
    pub mem_mb: u64,
    pub gpus: u32,
    pub jobs: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaBehaviour {
    pub borrow: bool,
    pub preempt: PreemptMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub name: String,
    pub description: String,
    pub selector: String,
    pub share_permille: u32,
    pub ceiling: QuotaCeiling,
    pub behaviour: QuotaBehaviour,
    pub created_at_micros: u64,
    pub created_by: UserId,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalReservation {
    pub principal: UserId,
    pub cpus: u32,
    pub mem_mb: u64,
    pub gpus: u32,
    pub jobs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReservations {
    pub by_principal: Vec<PrincipalReservation>,
    pub at_micros: u64,
}
