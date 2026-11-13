//! Jobs whose body travels in the request (`IpcRequest::RunScript`).
//!
//! Three ways to say *what to run* existed before this: a Library service
//! (a published, signed, versioned entry), an ad-hoc command (a string for
//! `bash -c`), and a workflow step's inline script. The third is the useful
//! shape and it was reachable only from inside a workflow.
//!
//! The build farm is what made that gap load-bearing. `meshnode cc` has to
//! ship a **preprocessed translation unit** — a megabyte of text that is the
//! whole input to one compile — to whichever node the scheduler picks. It
//! cannot go in `argv` (ARG_MAX), it cannot go in the environment (same
//! limit), and publishing a Library version per translation unit would leave
//! a signed, replicated entry behind for every `.c` file in every build.
//! `JobPayload::Script` already carries exactly this and already caches,
//! runs, streams and adopts like any other job; it simply had no door.
//!
//! ## Why this needs a version gate when a workflow step did not
//!
//! Every executor re-derives the submitter's authority from the signed
//! `IpcRequest` inside the job spec (MASTERPLAN §7.2) — it decodes it. A
//! node that has never heard of `RunScript` does not degrade gracefully
//! when handed one; `postcard` refuses the variant index and the job fails
//! with "malformed job authorization payload", which is true and useless.
//!
//! So a script job carries [`SCRIPT_AUTH`] on its environment and placement
//! filters on it, exactly as M12's `INV_DOC` gates an inventory. The marker
//! is on the *environment* rather than inferred from the payload because
//! `JobPayload::Script` is also what a workflow step dispatches, and those
//! have run on older executors since M10 — the payload is not what changed,
//! the envelope is.

/// Reserved job-environment key: this job's authorization envelope is an
/// [`crate::IpcRequest::RunScript`], so only a peer at
/// [`crate::FARM_MINOR`] can decode it.
///
/// Set by the origin at dispatch, never by a user: it is a statement about
/// the request that is being made, and a job that could set it by hand
/// could talk its way *out* of a gate rather than into one.
pub const SCRIPT_AUTH: &str = "MESHWARE_TASK_SCRIPT_AUTH";

/// Does this job need an executor that understands `RunScript`?
pub fn requires_script_support(env: &[(String, String)]) -> bool {
    env.iter().any(|(k, _)| k == SCRIPT_AUTH)
}

/// The marker, as an environment entry.
pub fn auth_marker() -> (String, String) {
    (SCRIPT_AUTH.to_string(), "1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_what_placement_filters_on() {
        assert!(!requires_script_support(&[]));
        assert!(!requires_script_support(&[("FOO".into(), "1".into())]));
        assert!(requires_script_support(&[auth_marker()]));
    }

    #[test]
    fn the_marker_is_reserved_so_a_job_cannot_set_it() {
        assert!(crate::is_reserved_env(SCRIPT_AUTH));
    }

    #[test]
    fn the_marker_never_reaches_a_cache_key() {
        // It says how the mesh should *route* this job, not what the job
        // computes. A key that included it would split every action into a
        // marked and an unmarked half for no reason at all.
        assert!(crate::cache::is_volatile(SCRIPT_AUTH));
    }
}
