//! Signing and publishing the mesh retention policy (M31 A1).
//!
//! Thin, and deliberately so — the *rules* live in
//! [`mesh_proto::retention`], because every node has to apply them at merge
//! time and only some of them ever hold a user key. What is here is the
//! half that needs a keypair: building a policy from what an operator typed,
//! and signing it.

use std::sync::Arc;

use mesh_proto::{
    OpBody, RetentionPolicy, RetentionTable, retention::parse_days, retention_reset_signing_bytes,
};

use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::state::StateEngine;

/// Apply `table=window` edits to a policy, leaving untouched tables alone.
///
/// **An edit rather than a replacement**, and that is the whole of why this
/// function exists. `meshnode retention set runs=30d` must not silently
/// reset `audit` to its default: an operator shortening one window has said
/// nothing at all about the others, and a tool that reads silence as
/// "restore the default" is a tool that quietly shortened the audit log
/// while somebody was tuning disk.
pub fn apply_edits(
    base: &RetentionPolicy,
    windows: &[String],
    floors: &[String],
) -> Result<RetentionPolicy> {
    let mut policy = base.clone();
    for (list, is_floor) in [(windows, false), (floors, true)] {
        for entry in list {
            let (name, value) = entry.split_once('=').ok_or_else(|| {
                CoreError::State(format!(
                    "'{entry}' is not a <table>=<window> pair (try runs=30d)"
                ))
            })?;
            let table = RetentionTable::parse(name.trim()).ok_or_else(|| {
                CoreError::State(format!(
                    "'{}' is not a retention table — try one of {}",
                    name.trim(),
                    RetentionTable::ALL
                        .iter()
                        .map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let days = parse_days(value).map_err(CoreError::State)?;
            let mut w = policy.window(table);
            if is_floor {
                w.floor_days = days;
            } else {
                w.days = days;
            }
            policy.set(table, w);
        }
    }
    Ok(policy)
}

/// Build the policy an operator's edits describe, on top of what the mesh
/// currently has.
pub fn edit_live(
    engine: &Arc<StateEngine>,
    windows: &[String],
    floors: &[String],
) -> Result<RetentionPolicy> {
    apply_edits(&engine.retention(), windows, floors)
}

/// Stamp and sign a policy with the admin's own key.
///
/// The daemon never holds this key (C6/§7.1), so the signature is made here
/// and re-verified on every node that applies the op — which is what makes
/// `meshnode audit` able to say who shortened the mesh's history, and makes
/// it provable rather than merely recorded.
pub fn sign(user: &Keypair, mut policy: RetentionPolicy, now_micros: u64) -> RetentionPolicy {
    policy.created_by = user.user_id();
    policy.created_at_micros = now_micros;
    policy.windows.sort_by_key(|(t, _)| *t);
    policy.signature = Vec::new();
    policy.signature = user.sign(&policy.signing_bytes()).to_vec();
    policy
}

/// Sign a reset back to the shipped defaults.
pub fn sign_reset(user: &Keypair, at_micros: u64) -> Vec<u8> {
    user.sign(&retention_reset_signing_bytes(&user.user_id(), at_micros))
        .to_vec()
}

/// Publish a client-built, client-signed policy. Used by the testkit and by
/// Director's embedded engine, which hold the key in process.
pub fn put(engine: &Arc<StateEngine>, policy: RetentionPolicy) -> Result<()> {
    if let Some(why) = policy.refusal() {
        return Err(CoreError::State(why));
    }
    engine.author_op(OpBody::RetentionPut {
        policy: Box::new(policy),
    })?;
    Ok(())
}

/// Build, sign and publish in one step.
pub fn put_as(
    engine: &Arc<StateEngine>,
    user: &Keypair,
    windows: &[String],
) -> Result<RetentionPolicy> {
    let policy = sign(
        user,
        edit_live(engine, windows, &[])?,
        mesh_proto::now_micros(),
    );
    put(engine, policy.clone())?;
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::RetentionWindow;

    #[test]
    fn an_edit_leaves_every_table_it_did_not_name_alone() {
        let mut base = RetentionPolicy::default();
        base.set(
            RetentionTable::Facts,
            RetentionWindow {
                days: 30,
                floor_days: 0,
            },
        );

        let edited = apply_edits(&base, &["runs=7d".into()], &[]).expect("edit");
        assert_eq!(edited.window(RetentionTable::Runs).days, 7);
        assert_eq!(
            edited.window(RetentionTable::Facts).days,
            30,
            "an untouched table must keep what it had, not revert to a default"
        );
        // And the one that matters most: `audit` is still forever.
        assert!(edited.window(RetentionTable::Audit).is_forever());
    }

    #[test]
    fn a_floor_is_edited_without_disturbing_the_window() {
        let base = RetentionPolicy::default();
        let edited = apply_edits(&base, &[], &["runs=7d".into()]).expect("edit");
        assert_eq!(edited.window(RetentionTable::Runs).days, 90, "unchanged");
        assert_eq!(edited.window(RetentionTable::Runs).floor_days, 7);
    }

    #[test]
    fn a_typo_says_what_the_tables_are() {
        let base = RetentionPolicy::default();
        let err = apply_edits(&base, &["run=7d".into()], &[]).expect_err("typo");
        let msg = err.to_string();
        assert!(msg.contains("runs"), "{msg}");
        assert!(msg.contains("job-output"), "{msg}");

        let err = apply_edits(&base, &["runs".into()], &[]).expect_err("no equals");
        assert!(err.to_string().contains("runs=30d"), "{err}");

        let err = apply_edits(&base, &["runs=soon".into()], &[]).expect_err("bad window");
        assert!(err.to_string().contains("90d"), "{err}");
    }

    #[test]
    fn publishing_below_the_audit_floor_is_refused_before_it_is_signed() {
        let base = RetentionPolicy::default();
        let edited = apply_edits(&base, &["audit=30d".into()], &[]).expect("edit");
        assert!(
            edited.refusal().is_some(),
            "the floor must refuse before anything reaches the log"
        );
    }
}
