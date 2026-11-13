//! What this node can run, probed rather than declared (T1 §4.2).
//!
//! ## The gap
//!
//! `capacity::of` probes cores, memory, disk, architecture, the dynamic
//! port range, lent volumes and attached devices, announces every one of
//! them as a `cap.*` tag, and `membership::audit_feasible_full` filters
//! placement on all of them.
//!
//! Nothing probed **what this node can run**. So a Python job went to a
//! node with no `python3`, and a container job went to a node with no
//! engine — both placed successfully, both discovering the truth at spawn,
//! as an error with no scheduling consequence and no sentence anywhere
//! saying *this machine cannot do that*.
//!
//! This is the probe. [`mesh_proto::runtimes`] is the vocabulary it speaks,
//! [`crate::capacity`] carries the answer into the node's tags, and
//! `membership` filters on it.
//!
//! ## Probes are filters, not promises
//!
//! `podman info` answering does not mean the next `podman run` will
//! succeed, and `python3 --version` answering does not mean the venv will
//! build. Detection removes nodes that *certainly* cannot; admission still
//! answers `Busy` and lets a job spill — the same split ports and volumes
//! already make, for the same reason: this side cannot see live node-local
//! state and should not pretend to.

use mesh_proto::Runtime;
use mesh_proto::runtimes::{ALL, RuntimeReport};

/// How long any one probe gets.
///
/// This runs on the capacity refresh, so a hung interpreter must read as
/// broken rather than stall a node's announcement of itself.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// What the container probe needs to know from the node's configuration.
#[derive(Debug, Clone, Default)]
pub struct ContainerOptions {
    /// `[runtime] container_plugin`, when the operator set one. It wins the
    /// resolution order, so it wins the report.
    pub plugin: Option<String>,
    /// `[runtime] container_engine`, when the operator named one. It
    /// reorders the search; it never restricts it.
    pub prefer: Option<crate::engines::EngineKind>,
    /// Where this node's own podman service listens, when it has one
    /// ([`crate::layout::Layout::engine_socket`]).
    pub engine_socket: Option<std::path::PathBuf>,
}

/// Probe every runtime this build knows about.
///
/// Order is [`ALL`] order, which is the order a person should read them:
/// the two that need nothing, then the two that need an interpreter, then
/// the one that needs an engine.
pub async fn detect(opts: &ContainerOptions) -> Vec<RuntimeReport> {
    let mut out = Vec::with_capacity(ALL.len());
    for runtime in ALL {
        out.push(match runtime {
            Runtime::Exec => exec_report(),
            Runtime::Bash => {
                tool_report(
                    Runtime::Bash,
                    "bash",
                    "install bash (it is on essentially every Linux; a container \
                 base image without it is the usual reason this is missing)",
                )
                .await
            }
            Runtime::Python => {
                tool_report(
                    Runtime::Python,
                    "python3",
                    "install python3 (`dnf install python3`, `apt install python3`)",
                )
                .await
            }
            Runtime::Ansible => {
                tool_report(
                    Runtime::Ansible,
                    "ansible-playbook",
                    "install ansible (`dnf install ansible`, `pipx install ansible`) — \
                 or pin it in a bundle's requirements.txt, which this node builds \
                 into a venv per service",
                )
                .await
            }
            Runtime::Container => {
                let found =
                    crate::engines::detect(opts.prefer, opts.engine_socket.as_deref()).await;
                crate::engines::report(&found, opts.plugin.as_deref())
            }
        });
    }
    out
}

/// `exec` needs nothing but the kernel.
///
/// **And that is load-bearing rather than a convenience.** It is why every
/// node on this build announces a non-empty `cap.rt`, which is what lets an
/// *absent* tag mean "too old to say" rather than "can run nothing" — see
/// [`mesh_proto::runtimes::from_tags`]. If this ever became conditional,
/// that inference would quietly become wrong.
fn exec_report() -> RuntimeReport {
    RuntimeReport::ready(Runtime::Exec, "the kernel")
}

/// Probe one program that answers `--version`.
///
/// The three states, told apart the way Nomad tells them apart: not on
/// `PATH` is *absent*; on `PATH` but refusing to run is *broken*. An
/// operator who is told "absent" about something they installed goes and
/// installs it again.
async fn tool_report(runtime: Runtime, program: &str, remedy: &str) -> RuntimeReport {
    let child = tokio::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return RuntimeReport::absent(runtime, remedy);
        }
        Err(e) => {
            return RuntimeReport::broken(runtime, format!("{program}: {e}"), remedy);
        }
    };
    let out = match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return RuntimeReport::broken(runtime, format!("{program}: {e}"), remedy),
        Err(_) => {
            return RuntimeReport::broken(
                runtime,
                format!(
                    "{program} did not answer within {}s",
                    PROBE_TIMEOUT.as_secs()
                ),
                remedy,
            );
        }
    };
    if !out.status.success() {
        return RuntimeReport::broken(
            runtime,
            format!("{program} --version exited {}", out.status),
            remedy,
        );
    }
    RuntimeReport::ready(runtime, version_line(program, &out.stdout, &out.stderr))
}

/// The one line worth showing from a `--version` answer.
///
/// `bash --version` prints five lines of copyright; `ansible-playbook
/// --version` prints seven lines of paths. The first line is the version in
/// every case, and the rest is noise in a table. The program name is put
/// back in front only when the tool did not already say it, so this reads
/// as `python3 3.13.1` and not `python3 Python 3.13.1`.
fn version_line(program: &str, stdout: &[u8], stderr: &[u8]) -> String {
    // Some tools print their version to stderr. `bash --version` does not,
    // but enough do that reading only stdout would report a working tool
    // as having no version.
    let text = if stdout.iter().any(|b| !b.is_ascii_whitespace()) {
        String::from_utf8_lossy(stdout).into_owned()
    } else {
        String::from_utf8_lossy(stderr).into_owned()
    };
    let first = text.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        None => program.to_string(),
        Some(line) => {
            let short = shorten(line);
            // Match on the program's *stem* — `python3` against "Python
            // 3.13.1". The version number is part of the program name on
            // disk and not part of what the program calls itself, so a
            // literal comparison prefixes it and prints `python3 Python
            // 3.13.1`.
            let stem = program.trim_end_matches(|c: char| c.is_ascii_digit());
            let named = !stem.is_empty()
                && short
                    .to_ascii_lowercase()
                    .contains(&stem.to_ascii_lowercase());
            if named {
                short
            } else {
                format!("{program} {short}")
            }
        }
    }
}

/// A version line, cut to something a table row can hold.
fn shorten(line: &str) -> String {
    const CAP: usize = 60;
    // `GNU bash, version 5.2.32(1)-release (x86_64-pc-linux-gnu)` — the
    // parenthetical is the build triple and says nothing useful here.
    let line = line.split(" (").next().unwrap_or(line).trim();
    if line.chars().count() > CAP {
        let cut: String = line.chars().take(CAP - 1).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::runtimes::Detection;

    #[test]
    fn exec_is_always_ready() {
        // The inference in `mesh_proto::runtimes::from_tags` — that an
        // absent `cap.rt` means an old node rather than a node that can run
        // nothing — rests entirely on this being unconditional.
        assert_eq!(exec_report().detection, Detection::Ready);
    }

    #[test]
    fn a_version_line_is_one_line_and_says_the_tool_once() {
        let bash = b"GNU bash, version 5.2.32(1)-release (x86_64-pc-linux-gnu)\n\
                     Copyright (C) 2022 Free Software Foundation, Inc.\n";
        assert_eq!(
            version_line("bash", bash, b""),
            "GNU bash, version 5.2.32(1)-release"
        );

        // The tool did not name itself, so the program name goes in front.
        assert_eq!(version_line("python3", b"3.13.1\n", b""), "python3 3.13.1");
        // …and when it did, it is not said twice — matched on the stem,
        // because the `3` is part of the name on disk and not part of what
        // the program calls itself.
        assert_eq!(
            version_line("python3", b"Python 3.13.1\n", b""),
            "Python 3.13.1"
        );
        assert_eq!(
            version_line("ansible-playbook", b"ansible-playbook [core 2.17.6]\n", b""),
            "ansible-playbook [core 2.17.6]"
        );
    }

    /// Some tools answer on stderr. Reading only stdout reported a working
    /// tool as having no version at all.
    #[test]
    fn a_version_on_stderr_is_still_found() {
        assert_eq!(
            version_line("java", b"", b"openjdk 21.0.2\n"),
            "java openjdk 21.0.2"
        );
        assert_eq!(version_line("nothing", b"", b""), "nothing");
    }

    #[test]
    fn a_long_version_line_is_cut() {
        let long = "x".repeat(200);
        assert!(shorten(&long).chars().count() <= 60);
    }

    /// A program that is not on `PATH` is *absent*, with advice — never
    /// "broken", which sends somebody to debug an installation they do not
    /// have.
    #[tokio::test]
    async fn a_missing_tool_is_absent_with_advice() {
        let r = tool_report(
            Runtime::Python,
            "meshware-no-such-program-xyz",
            "install it",
        )
        .await;
        assert_eq!(r.detection, Detection::Absent);
        assert_eq!(r.remedy, "install it");
        assert!(r.detail.is_empty());
    }

    /// A program that exists and fails is *broken*, and keeps the advice.
    #[tokio::test]
    async fn a_failing_tool_is_broken_not_absent() {
        // `false` is on every POSIX machine and exits 1 for any argument.
        let r = tool_report(Runtime::Bash, "false", "install bash").await;
        assert_eq!(r.detection, Detection::Broken);
        assert!(r.detail.contains("false"));
        assert!(!r.remedy.is_empty(), "broken still needs the remedy");
    }

    /// Every runtime gets an answer, in a stable order — the property the
    /// re-announce diff depends on.
    #[tokio::test]
    async fn every_runtime_is_reported_in_order() {
        let reports = detect(&ContainerOptions::default()).await;
        assert_eq!(reports.len(), ALL.len());
        let got: Vec<Runtime> = reports.iter().map(|r| r.runtime).collect();
        assert_eq!(got, ALL.to_vec());
        // …and `exec` is ready on whatever machine this is running on.
        assert!(reports[0].detection.is_ready());
    }

    /// A configured plugin is reported ready even where no engine is
    /// installed — the resolution order and the report must agree, or the
    /// status line contradicts the machine.
    #[tokio::test]
    async fn a_plugin_is_believed_over_a_missing_engine() {
        let opts = ContainerOptions {
            plugin: Some("/usr/libexec/mw-runc".into()),
            prefer: None,
            engine_socket: None,
        };
        let reports = detect(&opts).await;
        let container = reports
            .iter()
            .find(|r| r.runtime == Runtime::Container)
            .expect("a container report");
        assert!(container.detection.is_ready());
        assert!(container.detail.contains("mw-runc"));
    }
}
