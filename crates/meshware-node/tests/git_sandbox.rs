//! **The daemon runs `git` the way it runs a job** (SEC-21), tested against
//! a real `meshnode up`, because what is being checked is what the daemon
//! passes on from its own process environment.
//!
//! A watched source's `git` ran in the daemon with its whole environment,
//! no deadline, and no confinement, and what it fetched was rolled out to
//! every service the source bumps. The probe here is an `ssh://` source
//! whose ssh command is a script: it runs exactly where git runs whatever
//! it is told to, and reports what it could see.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROBE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
if cat "$(cat "$dir/target")"/* >/dev/null 2>&1; then echo PROBE-KEY-READ >&2; else echo PROBE-KEY-DENIED >&2; fi
if [ -n "$MW_TEST_SECRET$MESHWARE_PASSPHRASE" ]; then echo PROBE-ENV-LEAKED >&2; else echo PROBE-ENV-CLEAN >&2; fi
echo "PROBE-PROMPT=$GIT_TERMINAL_PROMPT" >&2
if [ -e "$dir/hang" ]; then echo $$ > "$dir/pid"; sleep 30; fi
exit 1
"#;

fn meshnode(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_meshnode"));
    cmd.arg("--data-dir").arg(data);
    cmd
}

/// A node founded in `data` with `config` as its `meshnode.toml`, whose
/// process environment carries the probe and a secret it must not pass on.
struct Daemon {
    data: tempfile::TempDir,
    child: Child,
}

impl Daemon {
    fn start(name: &str, config: &str, probe: &Path) -> Daemon {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("meshnode.toml"), config).unwrap();
        let child = meshnode(data.path())
            .args(["up", "--listen", "127.0.0.1:0", "--mesh-name", name])
            .args(["--node-name", "alpha"])
            .env(
                "GIT_SSH_COMMAND",
                format!("sh {}", probe.join("probe.sh").display()),
            )
            .env("MW_TEST_SECRET", "hunter2")
            // Read only for a sealed key, which this one is not — and
            // never git's business either way (SEC-23).
            .env("MESHWARE_PASSPHRASE", "hunter2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Daemon { data, child };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !daemon.cli(&["status"]).0 {
            assert!(Instant::now() < deadline, "{name} did not come up");
            std::thread::sleep(Duration::from_millis(200));
        }
        daemon
    }

    fn cli(&self, args: &[&str]) -> (bool, String) {
        let out = meshnode(self.data.path())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    /// Register the probe source and sync it; the sync always fails, and
    /// its error carries what the probe saw.
    fn sync_through(&self, probe: &Path) -> String {
        std::fs::write(
            probe.join("target"),
            self.data
                .path()
                .join("identity")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        let (ok, text) = self.cli(&[
            "sources",
            "add-git",
            "probe",
            "--url",
            "ssh://git@example.invalid/repo.git",
        ]);
        assert!(ok, "{text}");
        let (ok, text) = self.cli(&["sources", "sync", "probe"]);
        assert!(!ok, "the probe source synced: {text}");
        text
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.contains(") Z "),
    }
}

#[test]
fn git_runs_confined_with_a_clean_environment_and_a_deadline() {
    let probe = tempfile::tempdir().unwrap();
    std::fs::write(probe.path().join("probe.sh"), PROBE).unwrap();

    // Confined, where this machine can confine.
    let text = {
        let d = Daemon::start("git-sandbox", "", probe.path());
        d.sync_through(probe.path())
    };
    assert!(text.contains("PROBE-ENV-CLEAN"), "{text}");
    assert!(text.contains("PROBE-PROMPT=0"), "{text}");
    let policy = mesh_core::sandbox::Policy {
        shim: Some(env!("CARGO_BIN_EXE_meshnode").into()),
        ..Default::default()
    };
    if mesh_core::sandbox::unavailable_reason(&policy).is_none() {
        assert!(
            text.contains("PROBE-KEY-DENIED"),
            "git could read the node key: {text}"
        );
    } else {
        eprintln!("no Landlock here; the confinement was not exercised");
    }

    // The same probe, unconfined, does read it: the probe works.
    let text = {
        let d = Daemon::start("git-open", "[runtime]\nsandbox = \"off\"\n", probe.path());
        d.sync_through(probe.path())
    };
    assert!(text.contains("PROBE-KEY-READ"), "{text}");
    assert!(text.contains("PROBE-ENV-CLEAN"), "{text}");

    // A fetch that hangs is stopped, with everything it started.
    // Unconfined, so the probe can say which process to look for — a
    // confined one cannot write outside its checkout.
    std::fs::write(probe.path().join("hang"), b"").unwrap();
    let d = Daemon::start(
        "git-deadline",
        "[runtime]\nsandbox = \"off\"\n\n[git]\ntimeout = \"2s\"\n",
        probe.path(),
    );
    let started = Instant::now();
    let text = d.sync_through(probe.path());
    assert!(text.contains("did not finish within 2s"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(20), "{text}");
    let pid: i32 = std::fs::read_to_string(probe.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(gone(pid), "the hung ssh ({pid}) outlived the deadline");
}
