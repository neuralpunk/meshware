//! The job sandbox, against a real kernel.
//!
//! `mesh_core::sandbox`'s unit tests pin what the *rules* say; this pins
//! what the *kernel* does with them. The node is an in-process founder
//! (mesh-testkit), told to re-exec the real `meshnode` binary as its
//! sandbox shim — the same seam the PTY tests use — and the job is a
//! shell script that tries the four things a job must not be able to do
//! and the one it must:
//!
//!   * create a file directly in the daemon's `$HOME` — refused;
//!   * write into the node's own data directory — refused, even though
//!     that directory sits under `/tmp` here, which is exactly the case
//!     that made `/tmp` read-only;
//!   * write into `/tmp` itself — refused;
//!   * write into its own `TMPDIR` — allowed, because that is inside the
//!     job's directory.
//!
//! On a kernel without Landlock the job reports `MESHWARE_SANDBOX=none`
//! and the test says so and passes: the sandbox is `auto`, and this is
//! what `auto` promised.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState};
use mesh_testkit::{founder_user_key, spawn_founder_with, stream_as};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_cannot_write_outside_its_own_directory() {
    let node = spawn_founder_with("alpha", "sandbox-mesh", vec![], |cfg| {
        cfg.runtime.sandbox.shim = Some(env!("CARGO_BIN_EXE_meshnode").into());
    })
    .await;
    let data_dir = node.config().data_dir.clone();

    let script = r#"
echo "sandbox=$MESHWARE_SANDBOX"
if touch "$HOME/.mw-sandbox-escape" 2>/dev/null; then echo HOME_ESCAPED; rm -f "$HOME/.mw-sandbox-escape"; else echo HOME_REFUSED; fi
if touch "$MW_DATA/escape" 2>/dev/null; then echo DATA_ESCAPED; rm -f "$MW_DATA/escape"; else echo DATA_REFUSED; fi
if touch "/tmp/mw-sandbox-escape.$$" 2>/dev/null; then echo TMP_ESCAPED; rm -f "/tmp/mw-sandbox-escape.$$"; else echo TMP_REFUSED; fi
if touch "$TMPDIR/scratch" 2>/dev/null; then echo TMPDIR_OK; else echo TMPDIR_REFUSED; fi
echo END
"#;
    let mut rx = stream_as(
        &node,
        founder_user_key(&node),
        IpcRequest::Run {
            command: script.into(),
            on: Some("alpha".into()),
            env: vec![(
                "MW_DATA".into(),
                data_dir.join("identity").to_string_lossy().into_owned(),
            )],
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;

    let mut out = String::new();
    let mut state = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while state.is_none() {
        let frame = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the job finished in time");
        match frame {
            Some(IpcResponse::LogChunk { data, .. }) => {
                out.push_str(&String::from_utf8_lossy(&data))
            }
            Some(IpcResponse::JobDone { state: s, .. }) => state = Some(s),
            Some(IpcResponse::Error { message }) | Some(IpcResponse::Denied { message }) => {
                panic!("run refused: {message}")
            }
            Some(_) => {}
            None => break,
        }
    }
    assert_eq!(state, Some(JobState::Succeeded), "job output:\n{out}");
    assert!(out.contains("END"), "the whole script ran:\n{out}");

    let status = out
        .lines()
        .find_map(|l| l.strip_prefix("sandbox="))
        .unwrap_or("")
        .trim()
        .to_string();
    match status.as_str() {
        "full" | "partial" => {
            assert!(
                out.contains("HOME_REFUSED"),
                "$HOME must not be writable:\n{out}"
            );
            assert!(
                out.contains("DATA_REFUSED"),
                "the node's data must not be writable:\n{out}"
            );
            assert!(
                out.contains("TMP_REFUSED"),
                "/tmp must not be writable:\n{out}"
            );
            assert!(
                out.contains("TMPDIR_OK"),
                "the job's own TMPDIR must be writable:\n{out}"
            );
        }
        other => {
            // No Landlock here (an old kernel, a container that filters the
            // syscall): `auto` runs the job plainly and the child was told.
            eprintln!("sandbox status {other:?} — kernel has no Landlock; rules not exercised");
            assert!(out.contains("TMPDIR_OK") || other == "none", "{out}");
        }
    }
}
