//! Mesh-wide load shedding (N6 §12): two switches, one record.
//!
//! Nomad has `-reject-job-registration` and `-pause-eval-broker`. meshware
//! already had the per-deployment half — `deployment pause` — and nothing
//! mesh-wide, so the answer to *"stop everything, we are on fire"* was to
//! go round every node.
//!
//! ## The two switches, and why they are not Nomad's two
//!
//! A literal port would be "reject registrations" and "pause the eval
//! broker", and meshware has no eval broker. The pair that carries the same
//! meaning here splits on **who decided to start the work**:
//!
//! - [`Shed::submissions`] — work a *person* submits. `meshnode run`, a
//!   service run, a fan-out, a benchmark. Refused at the origin with a
//!   sentence naming who shed and why.
//! - [`Shed::reconcile`] — work the *mesh* starts on its own: deployment
//!   reconciliation and schedule firing. This is the one that matters when
//!   a mesh is thrashing, because it is the half that keeps starting things
//!   while you are trying to stop them.
//!
//! **Neither stops running work.** A shed is about what starts next; jobs
//! already executing finish, and a job that is mid-stream keeps streaming.
//! Killing running work is `meshnode drain`, which is a different verb
//! because it is a different decision.
//!
//! ## No bypass, on purpose
//!
//! There is no admin exemption. A shed that the person who set it can walk
//! through is not a shed, it is a suggestion — and the one thing an
//! operator needs to be able to believe at 3am is that the switch did what
//! it says. Reads are untouched (`status`, `nodes`, `top`, `doctor`, logs,
//! the registry, the ledgers), and so are sessions: `meshnode shell` is how
//! you fix the thing, not a way to put load on it.

use serde::{Deserialize, Serialize};

use crate::UserId;
use crate::rbac::tagged_bytes;

/// The mesh-wide shed record. **One of it**, LWW by HLC.
///
/// A singleton rather than a per-node flag, and that is the whole point:
/// the failure it exists to prevent is a mesh where the switch was flipped
/// on four nodes out of nine and the other five kept accepting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shed {
    /// Refuse work a person submits.
    pub submissions: bool,
    /// Stop deployment reconciliation and schedule firing.
    pub reconcile: bool,
    /// Why, in the operator's words. Shown in every refusal this causes —
    /// a job refused with no reason is a bug report, and a job refused with
    /// *"draining the SAN, back by 14:00"* is an answer.
    pub reason: String,
    /// The admin who set it; their signature authorizes it.
    pub created_by: UserId,
    pub created_at_micros: u64,
    pub signature: Vec<u8>,
}

impl Default for Shed {
    fn default() -> Self {
        Shed {
            submissions: false,
            reconcile: false,
            reason: String::new(),
            // `UserId` has no `Default` on purpose — an all-zero principal
            // is not a person — so the empty record names the zero id
            // explicitly and `sheds()` is what decides whether it means
            // anything.
            created_by: UserId([0; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        }
    }
}

impl Shed {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-shed-v1", &unsigned)
    }

    /// Is anything actually shed? A record with both switches clear is how
    /// a shed is lifted, and it must read as "nothing is held back".
    pub fn sheds(&self) -> bool {
        self.submissions || self.reconcile
    }

    /// The refusal a submission gets, as a sentence.
    ///
    /// Built here rather than at each call site so every refusal says the
    /// same thing — there are four places that can refuse and an operator
    /// should not be able to tell which one they hit.
    pub fn refusal(&self) -> String {
        let mut s = String::from(
            "new submissions are shed mesh-wide: an admin has asked every node to stop \
             taking work",
        );
        if !self.reason.trim().is_empty() {
            s.push_str(&format!(" — {}", self.reason.trim()));
        }
        s.push_str(". Running jobs are unaffected. `meshnode shed status` says who set it; ");
        s.push_str("`meshnode shed clear` lifts it.");
        s
    }
}

/// How ready the mesh is to honour a shed (N6 §12).
///
/// **The dangerous direction, named.** Every other capability minor in this
/// product fails towards *less* happening on an old node: an old node
/// publishes no registry, applies no rollout policy, enforces no presence.
/// This one fails the other way — a node that cannot read the record keeps
/// accepting work — so the readout is not a nicety, it is the difference
/// between a switch and the appearance of one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShedReadiness {
    pub members: u32,
    /// Members on a build that honours the record.
    pub honouring: u32,
    /// Names of the ones that do not, so the sentence can be specific.
    pub behind: Vec<String>,
}

impl ShedReadiness {
    pub fn complete(&self) -> bool {
        self.behind.is_empty()
    }

    /// One line, or `None` when there is nothing to warn about.
    pub fn warning(&self) -> Option<String> {
        if self.complete() {
            return None;
        }
        Some(format!(
            "{} of {} nodes will honour this shed. {} will keep accepting work: {}. \
             Unlike every other capability gate in meshware, this one fails *open* — \
             upgrade them or drain them.",
            self.honouring,
            self.members,
            self.behind.len(),
            self.behind.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_record_sheds_nothing() {
        let s = Shed::default();
        assert!(!s.sheds());
        assert!(
            Shed {
                submissions: true,
                ..Default::default()
            }
            .sheds()
        );
        assert!(
            Shed {
                reconcile: true,
                ..Default::default()
            }
            .sheds()
        );
    }

    #[test]
    fn the_refusal_carries_the_reason_and_the_way_out() {
        let s = Shed {
            submissions: true,
            reason: "  draining the SAN, back by 14:00  ".into(),
            ..Default::default()
        };
        let r = s.refusal();
        assert!(r.contains("draining the SAN, back by 14:00"), "{r}");
        assert!(r.contains("shed clear"), "{r}");
        assert!(r.contains("Running jobs are unaffected"), "{r}");
        // And with no reason it is still a complete sentence rather than a
        // dangling dash.
        let bare = Shed {
            submissions: true,
            reason: "   ".into(),
            ..Default::default()
        }
        .refusal();
        assert!(!bare.contains("—"), "{bare}");
    }

    #[test]
    fn the_signature_covers_everything_but_itself() {
        let a = Shed {
            submissions: true,
            reason: "one".into(),
            ..Default::default()
        };
        let mut b = a.clone();
        b.signature = vec![9; 64];
        assert_eq!(
            a.signing_bytes(),
            b.signing_bytes(),
            "the signature is not covered"
        );
        let mut c = a.clone();
        c.reason = "two".into();
        assert_ne!(
            a.signing_bytes(),
            c.signing_bytes(),
            "the reason must be covered"
        );
        let mut d = a.clone();
        d.reconcile = true;
        assert_ne!(
            a.signing_bytes(),
            d.signing_bytes(),
            "the switches must be covered"
        );
    }

    #[test]
    fn readiness_says_who_will_ignore_it() {
        let all = ShedReadiness {
            members: 4,
            honouring: 4,
            behind: Vec::new(),
        };
        assert!(all.complete());
        assert_eq!(all.warning(), None);

        let some = ShedReadiness {
            members: 4,
            honouring: 2,
            behind: vec!["edge-7".into(), "anvil".into()],
        };
        let w = some.warning().expect("a warning");
        assert!(w.contains("edge-7") && w.contains("anvil"), "{w}");
        assert!(w.contains("fails *open*"), "{w}");
    }
}
