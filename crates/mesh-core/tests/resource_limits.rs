//! **Declared resource limits are real ceilings** (E2.1).
//!
//! Before this, `--cpus 2 --mem 4G` was an input to *admission* and nothing
//! else: the scheduler used it to decide where a job fit, and the executor
//! then let the job use every core and all the memory on the box. That is a
//! correctness bug before it is a security one — C1's fair share and quotas
//! are computed from *reserved* resources, so a principal reserving two
//! cores and using sixty-four takes from everybody while being charged for
//! two.
//!
//! ## Why these tests skip rather than fail
//!
//! Enforcement needs a **delegated, writable cgroup v2 subtree**. A
//! developer laptop running `cargo test` from a shell usually has one (the
//! session scope); CI in a container usually does not. A test that failed
//! there would be a test everybody learns to ignore, so these skip with a
//! printed reason — and [`the_node_is_honest_about_whether_it_enforces`]
//! runs everywhere, because *the honesty* is the part that must never
//! regress. A node that cannot enforce and says it can is worse than one
//! that cannot enforce.
#![allow(clippy::unwrap_used)]

use mesh_core::cgroup::Delegation;
use mesh_proto::Requirements;

/// A delegation to test against, or `None` with the reason printed.
fn delegation() -> Option<Delegation> {
    match Delegation::probe() {
        Ok(d) => match d.prepare() {
            Ok(()) => Some(d),
            Err(why) => {
                println!("SKIP: cannot prepare a delegated subtree here — {why}");
                None
            }
        },
        Err(why) => {
            println!("SKIP: no cgroup delegation here — {why}");
            None
        }
    }
}

/// **The test that runs everywhere.**
///
/// Whatever this machine can or cannot do, the node's *answer* about it
/// must be usable: either it enforces and names the controllers, or it does
/// not and gives a reason an operator can act on. Silence, or a bare
/// `false`, sends somebody to read source code.
#[test]
fn the_node_is_honest_about_whether_it_enforces() {
    match Delegation::probe() {
        Ok(d) => {
            assert!(
                !d.controllers().is_empty(),
                "claiming enforcement with no controllers is the dishonest state"
            );
            for c in d.controllers() {
                assert!(
                    ["cpu", "memory", "pids"].contains(&c.as_str()),
                    "unexpected controller {c}"
                );
            }
        }
        Err(why) => {
            assert!(why.len() > 20, "a refusal must be actionable, got: {why}");
            assert!(
                why.contains("cgroup")
                    || why.contains("Delegate")
                    || why.contains("Linux")
                    || why.contains("enforce"),
                "the reason should name what is wrong: {why}"
            );
        }
    }
}

/// The limits a job asks for are written to its cgroup, in the units the
/// kernel expects. `--cpus 2` is two whole cores over the standard period,
/// not a share weight and not a percentage.
#[test]
fn declared_limits_reach_the_kernel_in_the_right_units() {
    let Some(d) = delegation() else { return };
    let reqs = Requirements {
        cpus: 2,
        mem_mb: 64,
        disk_mb: 0,
    };
    let cg = d
        .job("job-e21-units", &reqs)
        .expect("a job cgroup should be creatable in a delegated subtree");
    let read = |f: &str| std::fs::read_to_string(cg.path().join(f)).map(|s| s.trim().to_string());

    if d.controllers().iter().any(|c| c == "cpu") {
        assert_eq!(
            read("cpu.max").unwrap(),
            "200000 100000",
            "--cpus 2 must be two whole cores over the 100ms period"
        );
    }
    if d.controllers().iter().any(|c| c == "memory") {
        assert_eq!(read("memory.max").unwrap(), (64 * 1024 * 1024).to_string());
        // **The line that makes the ceiling a ceiling.** Without it the
        // kernel swaps instead of OOM-killing and `memory.max` silently
        // becomes a performance hint — measured, not assumed.
        assert_eq!(
            read("memory.swap.max").unwrap(),
            "0",
            "memory.max without memory.swap.max=0 does not bite"
        );
    }
    if d.controllers().iter().any(|c| c == "pids") {
        assert_ne!(
            read("pids.max").unwrap(),
            "max",
            "a pid ceiling should be finite"
        );
    }
}

/// A job that asked for nothing is bounded by the node, exactly as it was
/// before this feature existed. An unset requirement must not become `0`.
#[test]
fn a_job_that_asked_for_nothing_is_not_given_a_zero_limit() {
    let Some(d) = delegation() else { return };
    let cg = d
        .job("job-e21-unset", &Requirements::default())
        .expect("creatable");

    if d.controllers().iter().any(|c| c == "memory") {
        let v = std::fs::read_to_string(cg.path().join("memory.max")).unwrap();
        assert_eq!(
            v.trim(),
            "max",
            "an unset --mem must leave the ceiling at the node's, never at 0"
        );
    }
    if d.controllers().iter().any(|c| c == "cpu") {
        let v = std::fs::read_to_string(cg.path().join("cpu.max")).unwrap();
        assert!(
            v.starts_with("max"),
            "an unset --cpus must leave the quota unset, got {v:?}"
        );
    }
}

/// **The one that matters: the limit actually kills.**
///
/// Everything above checks that the right numbers reached the right files.
/// This checks the kernel does something about them — which is a different
/// claim, and the one that was false in the first version of this module
/// because `memory.swap.max` was missing.
#[test]
fn a_process_over_its_memory_limit_is_killed() {
    let Some(d) = delegation() else { return };
    if !d.controllers().iter().any(|c| c == "memory") {
        println!("SKIP: no memory controller in this subtree");
        return;
    }
    let cg = d
        .job(
            "job-e21-oom",
            &Requirements {
                cpus: 0,
                mem_mb: 32,
                disk_mb: 0,
            },
        )
        .expect("creatable");

    // A child that joins the cgroup and then asks for far more than the
    // ceiling. `sh` so there is no dependency on a language runtime beyond
    // what any Linux box has.
    let script = format!(
        "echo 0 > {}/cgroup.procs || exit 3; \
         exec head -c 512M /dev/zero | tail -c 512M > /dev/null",
        cg.path().display()
    );
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .status()
        .expect("spawn sh");

    assert!(
        !status.success() || cg.was_oom_killed(),
        "a job 16x over its declared memory limit must not complete successfully \
         (status {status:?}, oom_kill seen: {})",
        cg.was_oom_killed()
    );
}

/// A cgroup is removed when its guard drops, so a busy node does not
/// accumulate thousands of directories under `/sys/fs/cgroup`.
#[test]
fn a_job_cgroup_is_removed_when_the_job_is_done() {
    let Some(d) = delegation() else { return };
    let path = {
        let cg = d
            .job("job-e21-drop", &Requirements::default())
            .expect("creatable");
        let p = cg.path().to_path_buf();
        assert!(p.exists());
        p
    };
    assert!(
        !path.exists(),
        "an empty job cgroup should be removed when the job finishes"
    );
}

/// A job id arrives over the wire, so it is treated as untrusted input on
/// the way to becoming a path segment.
#[test]
fn a_hostile_job_id_cannot_escape_the_jobs_directory() {
    let Some(d) = delegation() else { return };
    let cg = d.job("../../../etc/passwd", &Requirements::default());
    if let Some(cg) = cg {
        let p = cg.path();
        assert!(
            p.to_string_lossy().contains("/jobs/"),
            "a job cgroup must stay under jobs/, got {}",
            p.display()
        );
        assert!(!p.to_string_lossy().contains(".."));
    }
}
