//! Sessions: the fabric (M17).
//!
//! A session is a **transport-owned bidirectional stream**, and it is the
//! only thing in the system besides a relay registration that reads bytes
//! from a peer after the request frame (M17 §3.3, R1/R2). Everything else
//! on the wire is request-in, responses-out with the request half already
//! finished — which is why nothing interactive could exist before this.
//!
//! The shape is deliberately the one the relay data plane already proves in
//! production over NAT and relays: one `Request::SessionOpen` frame, then
//! [`SessionFrame`]s initiator→host and [`SessionServerFrame`]s host→
//! initiator for the life of the stream, carried by the same
//! length-delimited postcard codec as everything else.
//!
//! ## What a session is not
//!
//! Not a job (§3.5). No `JobId`, no spool, no `JobUpsert` op, no executor
//! slot. A human at a prompt for an hour is not a unit of scheduled work,
//! a `JobSummary` for it would replicate an hour of terminal output to
//! every member, and closing the stream already kills the process group.
//! What a session *does* get is the audit log.
//!
//! ## Authorization
//!
//! [`SessionSpec::auth`] is the initiator's ordinary `SignedIpc` envelope,
//! exactly as `JobSpec::auth` is, and the **host** re-verifies it against
//! its own materialized state before the handler takes the stream — the
//! MASTERPLAN's "enforcement happens on the executing node", with no new
//! enforcement point. The envelope's payload decodes to
//! `IpcRequest::SessionOpen`, whose `kind` must equal [`SessionSpec::kind`]:
//! the signature covers what the user asked for, so a compromised origin
//! cannot upgrade a port-forward into a shell.

use serde::{Deserialize, Serialize};

use crate::ids::UserId;
use crate::ipc::SignedIpc;

/// Largest session payload frame, in bytes.
///
/// A session is a pipe, and a pipe with no ceiling is a memory bug waiting
/// for a `tar -cf -` of a large directory. QUIC's own flow control is the
/// backpressure (§3.7); this only bounds a single frame, so a peer cannot
/// make the other end allocate arbitrarily from one `read_frame`.
pub const MAX_SESSION_FRAME: usize = 256 * 1024;

/// Bytes read from a source before a `Data` frame is emitted. Well under
/// [`MAX_SESSION_FRAME`], because a shell wants small frames promptly and a
/// file copy fills this every time regardless.
pub const SESSION_CHUNK: usize = 32 * 1024;

/// Default cap on concurrent sessions one peer may hold on a host
/// (`[sessions] max_per_peer`). A forward is a tunnel and tunnels grow
/// (§3.8); the refusal names the cap.
pub const DEFAULT_MAX_SESSIONS_PER_PEER: u32 = 8;

/// Largest path a `Cp` session may name, bounding what an initiator can
/// make a host allocate before it has proved anything.
pub const MAX_SESSION_PATH: usize = 4096;

/// What kind of session this is — and therefore what the `Data` frames
/// mean, which side speaks first, and what a reserved tag does to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    /// Interactive PTY (§3.6). `command` is `None` for the user's login
    /// shell; `Some` runs one command under a terminal, which is what a
    /// Director terminal panel opens.
    Shell {
        command: Option<String>,
        /// The client's `TERM`, passed through so curses programs agree
        /// with the terminal actually rendering them.
        term: String,
        cols: u16,
        rows: u16,
    },
    /// Non-interactive: stdin piped in, stdout/stderr piped back, exit code
    /// returned. No PTY, so `tar -cf - /etc` is byte-exact.
    Exec { command: String },
    /// Push a file to the host. The `Data` frames are the file; the
    /// terminal [`SessionFrame::Digest`] is what lets both ends assert the
    /// copy landed intact (§3.7).
    CpTo {
        path: String,
        /// Unix mode bits to create the file with.
        mode: u32,
        /// Expected length, so the host can refuse before it writes.
        size: u64,
    },
    /// Pull a file from the host. The host answers
    /// [`SessionServerFrame::Meta`], then the body, then a digest.
    CpFrom { path: String },
    /// One accepted TCP connection, dialed on the host and spliced to this
    /// stream (§3.8). `forward` and `expose` are the same machinery seen
    /// from opposite ends — the host is simply whoever accepted the
    /// session — which is why there is no reverse request.
    Forward { host: String, port: u16 },
    /// A terminal **inside one container instance** on the host (Phase B,
    /// E3) — `meshnode deployment exec`, Nomad's `alloc exec`.
    ///
    /// Appended, never inserted: variants travel by index. Governed exactly
    /// as [`SessionKind::Shell`] is — the same grant, reason, audit action
    /// and session policy — because a prompt in a container on a node is no
    /// more than a prompt on that node. What differs is only what the host
    /// spawns: its own container engine's `exec`, into a container it has
    /// checked is a running instance it executes.
    Instance {
        /// The instance's job id, short form — the `<job>` in the
        /// container's `meshware-<job>` name.
        job: String,
        /// Run this instead of `sh`.
        command: Option<String>,
        term: String,
        cols: u16,
        rows: u16,
    },
}

impl SessionKind {
    /// Is this kind part of M17 v1?
    ///
    /// v1 is **the terminal and nothing else**. `Exec`, `CpTo`, `CpFrom`
    /// and `Forward` are built and work, and are deliberately not offered:
    /// a first release whose security story is one sentence — *an
    /// authorized operator can get a prompt on an authorized node, and
    /// every one is audited* — is a story that survives a security review.
    /// Copying files and tunnelling ports each widen that sentence, and
    /// neither has a customer asking for it yet.
    ///
    /// The variants stay on the wire at their existing indices rather than
    /// being deleted, because removing an enum variant is a protocol
    /// change and re-adding one later would be a second protocol change.
    /// Turning one back on is deleting a refusal.
    pub fn is_offered(&self) -> bool {
        // Phase B, E3: a terminal inside a container instance is a terminal,
        // offered and governed exactly as the one on the node is.
        matches!(
            self,
            SessionKind::Shell { .. } | SessionKind::Instance { .. }
        )
    }

    /// The audit action this kind records, in the existing dotted
    /// vocabulary. There is one audit log, and a shell is in it (§3.5).
    pub fn audit_action(&self) -> &'static str {
        match self {
            SessionKind::Shell { .. } | SessionKind::Exec { .. } | SessionKind::Instance { .. } => {
                "shell.open"
            }
            SessionKind::CpTo { .. } | SessionKind::CpFrom { .. } => "session.cp",
            SessionKind::Forward { .. } => "session.forward",
        }
    }

    /// The audit action recorded at teardown, with duration and exit.
    pub fn close_action(&self) -> &'static str {
        match self {
            SessionKind::Shell { .. } | SessionKind::Exec { .. } | SessionKind::Instance { .. } => {
                "shell.close"
            }
            SessionKind::CpTo { .. } | SessionKind::CpFrom { .. } => "session.cp",
            SessionKind::Forward { .. } => "session.forward",
        }
    }

    /// Does this kind spawn a process on the host?
    ///
    /// The whole of §3.9 turns on this one question. The reserved tags
    /// govern *execution*, never participation (D6), so `no-execute` and
    /// `draining` refuse a shell and allow a copy — a node that has stopped
    /// taking work has not stopped being reachable.
    pub fn spawns_a_process(&self) -> bool {
        matches!(
            self,
            SessionKind::Shell { .. } | SessionKind::Exec { .. } | SessionKind::Instance { .. }
        )
    }

    /// A one-line description for the audit log and the refusal message.
    pub fn describe(&self) -> String {
        match self {
            SessionKind::Instance {
                job, command: None, ..
            } => format!("shell in instance {job}"),
            SessionKind::Instance {
                job,
                command: Some(c),
                ..
            } => format!("shell in instance {job}: {}", preview(c)),
            SessionKind::Shell { command: None, .. } => "shell".to_string(),
            SessionKind::Shell {
                command: Some(c), ..
            } => format!("shell: {}", preview(c)),
            SessionKind::Exec { command } => format!("exec: {}", preview(command)),
            SessionKind::CpTo { path, size, .. } => format!("cp to {path} ({size} bytes)"),
            SessionKind::CpFrom { path } => format!("cp from {path}"),
            SessionKind::Forward { host, port } => format!("forward to {host}:{port}"),
        }
    }

    /// Structural limits, checked on the host before anything is spawned or
    /// opened. Returns the refusal, if there is one.
    pub fn refusal(&self) -> Option<String> {
        if !self.is_offered() {
            return Some(format!(
                "{} is not offered in this release — M17 ships the terminal only",
                kind_word(self)
            ));
        }
        match self {
            SessionKind::CpTo { path, .. } | SessionKind::CpFrom { path } => {
                if path.len() > MAX_SESSION_PATH {
                    return Some(format!(
                        "path is longer than the {MAX_SESSION_PATH}-byte limit"
                    ));
                }
                if path.is_empty() {
                    return Some("no path given".into());
                }
                None
            }
            SessionKind::Forward { host, port } => {
                if host.is_empty() {
                    return Some("no forward target given".into());
                }
                if *port == 0 {
                    return Some("port 0 is not a forward target".into());
                }
                None
            }
            // The job id becomes part of a container name and an argument to
            // the engine: refused here, structurally, before anything looks
            // it up — letters, digits, `-` and `_`, as a short id is.
            SessionKind::Instance { job, .. } => {
                if job.is_empty()
                    || job.len() > 64
                    || !job
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Some(format!("{job:?} is not an instance's job id"));
                }
                None
            }
            SessionKind::Shell { .. } | SessionKind::Exec { .. } => None,
        }
    }
}

/// `shell` | `exec` | `cp` | `forward` | `instance shell`.
pub fn kind_word(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::Instance { .. } => "instance shell",
        SessionKind::Shell { .. } => "shell",
        SessionKind::Exec { .. } => "exec",
        SessionKind::CpTo { .. } | SessionKind::CpFrom { .. } => "cp",
        SessionKind::Forward { .. } => "forward",
    }
}

fn preview(s: &str) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= 60 {
        one_line
    } else {
        let head: String = one_line.chars().take(57).collect();
        format!("{head}...")
    }
}

/// The request body of `Request::SessionOpen`.
///
/// `kind` is carried alongside `auth` rather than only inside it so the
/// host can refuse cheaply before decoding an envelope — and then checked
/// for equality against the decoded envelope, which is what makes the
/// signature meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub kind: SessionKind,
    /// The initiator's signed `IpcRequest::SessionOpen`, re-verified by the
    /// host node (§3.4).
    pub auth: SignedIpc,
}

/// Longest session reason the host will record.
pub const MAX_SESSION_REASON: usize = 200;

/// Which signal to deliver to the session's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSignal {
    Int,
    Term,
    Kill,
}

impl SessionSignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionSignal::Int => "int",
            SessionSignal::Term => "term",
            SessionSignal::Kill => "kill",
        }
    }
}

/// Which of the host's streams a `Data` frame came from.
pub const STREAM_STDOUT: u8 = 1;
pub const STREAM_STDERR: u8 = 2;

/// Initiator → host, for the life of the stream (§3.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionFrame {
    /// Opaque bytes: stdin, a file body, or TCP payload.
    Data(Vec<u8>),
    /// The client's terminal changed size. PTY sessions only.
    Resize { cols: u16, rows: u16 },
    /// Deliver a signal to the session's process group.
    Signal(SessionSignal),
    /// The initiator's write half is done. For `Exec` this closes the
    /// child's stdin; for `CpTo` it ends the file.
    Eof,
    /// BLAKE3 of everything the initiator sent, so the host can assert the
    /// copy landed intact before it renames the file into place (§3.7).
    Digest([u8; 32]),
}

/// Host → initiator, for the life of the stream (§3.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionServerFrame {
    /// The host authorized the session and the far end is live. Sent once,
    /// before any payload, so a client knows the difference between "still
    /// connecting" and "connected and quiet" — which for a port-forward
    /// listener is the difference between holding a TCP connection open and
    /// closing it.
    Opened,
    /// Opaque bytes from `stream` ([`STREAM_STDOUT`] / [`STREAM_STDERR`]).
    Data { stream: u8, bytes: Vec<u8> },
    /// A `CpFrom`'s file header, sent before its body.
    Meta { mode: u32, size: u64 },
    /// BLAKE3 of everything the host sent, terminating a `CpFrom` body.
    Digest([u8; 32]),
    /// The session's process ended.
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// The session is over and this is why. A refusal arrives as this and
    /// nothing else.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// The session policy entity (M17 v1)
// ---------------------------------------------------------------------------

/// What a session policy permits.
///
/// Two permissions, and the split is the one the enterprise guidance asks
/// for: an ordinary troubleshooting prompt, and a prompt that is allowed to
/// be root. They are separate so stronger controls can be attached to the
/// second later — fresh authentication, approval, a shorter TTL — without
/// burdening the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SessionPermission {
    /// Open a terminal, running as whatever user the daemon runs as.
    Shell,
    /// Open a terminal on a node whose daemon is uid 0 — a root prompt.
    ///
    /// Meaningful *only* there: the packaged daemon runs as an unprivileged
    /// `meshnode` user and cannot become anybody else, so on an ordinary
    /// node this grants nothing extra. It is not a way to *get* root; it is
    /// permission to have it where it was already unavoidable.
    ShellPrivileged,
}

impl SessionPermission {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionPermission::Shell => "shell",
            SessionPermission::ShellPrivileged => "shell-privileged",
        }
    }

    pub fn parse(s: &str) -> Option<SessionPermission> {
        match s.trim() {
            "shell" => Some(SessionPermission::Shell),
            "shell-privileged" | "shell_privileged" => Some(SessionPermission::ShellPrivileged),
            _ => None,
        }
    }
}

/// A session policy: **who** may open a terminal on **which** nodes.
///
/// Replicated as [`crate::OpBody::SessionPolicyPut`], admin-signed, and
/// verified at merge time on every node like any other governed entity.
/// Enforcement happens on the node being shelled into, against its own
/// materialized copy — the same rule RBAC follows, so there is no policy
/// server to be down.
///
/// ## Why an entity and not a `Role`
///
/// Appending a variant to `Role` is convergence-safe and *semantics*-unsafe
/// (F0, R7): a node without the code would carry the op and ignore the
/// role, which for authorization is split-brain — worse than no feature.
/// `Grant` cannot grow a field either; it is signed, and every existing
/// grant's signature covers its current shape.
///
/// ## Why this one fails closed
///
/// Sessions are **off unless something turns them on**, and a policy only
/// ever *grants*. So a node that cannot decode a newer policy — the exact
/// F0 case that makes a `Role` variant dangerous — grants nothing and
/// therefore refuses. The failure direction is the safe one, and it is the
/// reason this shape was chosen over the alternatives.
///
/// ## What it does not replace
///
/// A policy is **necessary, not sufficient**. The caller still needs the
/// ordinary runner grant covering the node (`rbac::RUNNERS` + `can_run_on`)
/// — the same grant `meshnode run` needs. Nobody gains a terminal by this
/// entity existing; they gain one by an admin writing a policy *and*
/// already being allowed to run work there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPolicy {
    /// Human-chosen, unique, and the handle every command uses.
    pub id: String,
    pub description: String,
    /// Username glob (`*`, `ada`, `ops-*`). Matched against the username in
    /// the caller's user record, so it survives key rotation.
    pub users: String,
    /// Node selector (`any`, `tag=dev`, `name=hermes`) — the same grammar
    /// grants and policies already use.
    pub nodes: String,
    /// What it permits. Empty permits nothing, which is a policy worth
    /// refusing at write time rather than puzzling over later.
    pub allow: Vec<SessionPermission>,
    /// When this policy stops applying. `None` = never.
    pub expires_at_micros: Option<u64>,
    pub created_at_micros: u64,
    /// The admin who wrote it; their signature authorizes it.
    pub created_by: UserId,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl SessionPolicy {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-session-policy-put-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    pub fn is_expired(&self, at_micros: u64) -> bool {
        self.expires_at_micros.is_some_and(|e| at_micros > e)
    }

    pub fn permits(&self, permission: SessionPermission) -> bool {
        self.allow.contains(&permission)
    }

    /// Why this policy cannot be written, if it cannot.
    pub fn refusal(&self) -> Option<String> {
        if self.id.trim().is_empty() {
            return Some("a session policy needs a name".into());
        }
        if self.allow.is_empty() {
            return Some(format!(
                "session policy {:?} permits nothing — remove it instead of writing an \
                 inert one",
                self.id
            ));
        }
        if self.users.trim().is_empty() {
            return Some("a session policy needs a user glob (`*` for everyone)".into());
        }
        if self.nodes.trim().is_empty() {
            return Some("a session policy needs a node selector (`any` for everywhere)".into());
        }
        None
    }
}

/// The bytes an admin signs to retire a session policy.
pub fn session_policy_remove_signing_bytes(id: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(b"mw-session-policy-remove-v1");
    buf.extend_from_slice(id.as_bytes());
    buf.extend_from_slice(&by.0);
    buf
}

/// Why a session ended, for the audit entry.
///
/// Typed rather than free text because it is the field an auditor filters
/// on: "how many sessions were cut off by the idle timeout last month" is a
/// question about this column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEnd {
    /// The far shell exited — somebody typed `exit`.
    UserExit,
    /// The client went away.
    Disconnected,
    /// No traffic for `[sessions] idle_timeout`.
    IdleTimeout,
    /// Open longer than `[sessions] max_duration`.
    MaxDuration,
    /// An operator ran `sessions close`.
    Terminated,
    /// The node drained.
    Drained,
    /// It never got started.
    Failed,
}

impl SessionEnd {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionEnd::UserExit => "user_exit",
            SessionEnd::Disconnected => "disconnected",
            SessionEnd::IdleTimeout => "idle_timeout",
            SessionEnd::MaxDuration => "max_duration",
            SessionEnd::Terminated => "terminated",
            SessionEnd::Drained => "drained",
            SessionEnd::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_tags_govern_execution_not_participation() {
        // §3.9: the whole table, as one assertion.
        assert!(
            SessionKind::Shell {
                command: None,
                term: "xterm".into(),
                cols: 80,
                rows: 24
            }
            .spawns_a_process()
        );
        assert!(
            SessionKind::Exec {
                command: "id".into()
            }
            .spawns_a_process()
        );
        assert!(
            !SessionKind::CpFrom {
                path: "/etc/hostname".into()
            }
            .spawns_a_process()
        );
        assert!(
            !SessionKind::Forward {
                host: "localhost".into(),
                port: 80
            }
            .spawns_a_process()
        );
    }

    #[test]
    fn only_the_terminal_is_offered_in_v1() {
        assert!(
            SessionKind::Shell {
                command: None,
                term: "xterm".into(),
                cols: 80,
                rows: 24
            }
            .is_offered()
        );
        for deferred in [
            SessionKind::Exec {
                command: "id".into(),
            },
            SessionKind::CpFrom {
                path: "/etc/hostname".into(),
            },
            SessionKind::Forward {
                host: "localhost".into(),
                port: 80,
            },
        ] {
            assert!(!deferred.is_offered(), "{deferred:?}");
            let refusal = deferred.refusal().expect("refused by name");
            assert!(refusal.contains("terminal only"), "{refusal}");
        }
    }

    #[test]
    fn a_preview_is_one_line_and_bounded() {
        let long = "echo ".to_string() + &"x".repeat(200);
        let d = SessionKind::Exec { command: long }.describe();
        assert!(d.len() < 80, "{d}");
        assert!(d.ends_with("..."));
        let multi = SessionKind::Exec {
            command: "a\nb".into(),
        }
        .describe();
        assert_eq!(multi, "exec: a b");
    }

    /// Phase B, E3: a shell inside an instance is offered, gated and audited
    /// as the terminal it is, and a job id that could not name a container
    /// is refused before anything looks it up.
    #[test]
    fn an_instance_shell_is_governed_as_a_shell() {
        let kind = |job: &str| SessionKind::Instance {
            job: job.into(),
            command: None,
            term: "xterm".into(),
            cols: 80,
            rows: 24,
        };
        let shell = SessionKind::Shell {
            command: None,
            term: "xterm".into(),
            cols: 80,
            rows: 24,
        };
        let ok = kind("job-7xk2mq4p");
        assert!(ok.is_offered());
        assert!(
            ok.spawns_a_process(),
            "no-execute and draining refuse it, as they refuse a shell"
        );
        assert_eq!(ok.audit_action(), shell.audit_action());
        assert_eq!(ok.close_action(), shell.close_action());
        assert_eq!(ok.refusal(), None);
        assert_eq!(ok.describe(), "shell in instance job-7xk2mq4p");
        let too_long = "a".repeat(65);
        for bad in ["", "job;rm -rf /", "../x", "a b", too_long.as_str()] {
            assert!(kind(bad).refusal().is_some(), "{bad:?} should be refused");
        }
    }
}
