//! The six lines that have to run in the child between fork and exec.
//!
//! ## Why this exists at all
//!
//! M17 §3.6 commits to `rustix` features rather than a new crate, and to
//! the workspace's `unsafe_code = "deny"` holding. Both survive contact
//! with reality, but not quite the way the design assumed: `openpt`,
//! `grantpt`, `unlockpt`, `ptsname` and `tcsetwinsize` are all safe rustix
//! calls made by the **parent**, and so is everything the client does — but
//! a pseudoterminal is not a terminal until some process makes it its
//! *controlling* terminal, and that is `setsid()` + `TIOCSCTTY` running in
//! the **child**, after the fork and before the exec.
//!
//! `std`'s only seam there is `CommandExt::pre_exec`, which is `unsafe fn`
//! (its closure runs between fork and exec, where almost nothing is legal),
//! and calling it would need an `unsafe` block in this crate.
//!
//! Without a controlling terminal the PTY still *works* — `isatty` is true,
//! readline behaves, curses draws — but `tty->pgrp` is never set, so the
//! line discipline has nobody to signal and **Ctrl-C does nothing**. A
//! shell where Ctrl-C does nothing is not a shell, so "skip it" was not on
//! the table.
//!
//! ## What it does instead
//!
//! The child is a re-exec of this very binary with [`SHIM_ARG`] as its
//! first argument. It inherits the pts slave as fds 0/1/2 — the parent set
//! that up with ordinary `Stdio`, no `unsafe` — and then runs, as an
//! ordinary process in `main`, the sequence that would otherwise have had
//! to happen inside a fork:
//!
//! 1. [`rustix::process::setsid`] — become a session leader,
//! 2. [`rustix::process::ioctl_tiocsctty`] — adopt fd 0 as the controlling
//!    terminal,
//! 3. `CommandExt::exec` — a *safe* function — become the shell.
//!
//! Every one of those is safe, the pid never changes, and the process group
//! `kill_process_group` later signals is the one `setsid` created.
//!
//! ## The cost
//!
//! Every binary that hosts a node has to call [`try_run`] as the first
//! statement of `main`, before any runtime is built. That is `meshnode` and
//! Director. Forgetting it does not corrupt anything — the shim would exec
//! the real `main`, see an unknown argument and exit — but the shell would
//! not open, so both call sites are worth keeping next to each other in
//! your head.

use std::ffi::OsString;

/// The argv[1] sentinel that turns this binary into the shim.
///
/// Double-underscored and undocumented in `--help` for the usual reason:
/// it is not a verb, it is an implementation detail of one, and a user who
/// runs it by hand gets a shell attached to their own terminal, which is
/// harmless and confusing in equal measure.
pub const SHIM_ARG: &str = "__meshware-pty";

/// Override for which binary the host re-execs as the shim.
///
/// Defaults to `current_exe()`, which is right in production: the process
/// hosting the node is `meshnode` or Director, and both call [`try_run`].
/// It is overridable for two real cases — a repackaging that splits the
/// binary, and an in-process test whose `current_exe()` is a test harness
/// that knows nothing about [`SHIM_ARG`].
pub const SHIM_ENV: &str = "MESHWARE_PTY_SHIM";

/// If this process was launched as the PTY shim, become the child the
/// session wants and never return. Otherwise return immediately.
///
/// Call it as the **first statement of `main`**, before building a runtime,
/// installing tracing, or reading a config: everything this does is
/// process-global, and the point is to have done nothing else first.
pub fn try_run() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    match args.next() {
        Some(first) if first == SHIM_ARG => {}
        _ => return,
    }
    let Some(program) = args.next() else {
        eprintln!("meshware: {SHIM_ARG} takes a program to run");
        std::process::exit(127);
    };
    let rest: Vec<OsString> = args.collect();
    exec(program, rest)
}

fn exec(program: OsString, args: Vec<OsString>) -> ! {
    use std::os::unix::process::CommandExt;

    // A new session, so this process can own a controlling terminal. It
    // fails with EPERM only if we are already a process-group leader, which
    // is why the host spawns the shim *without* `process_group(0)` — the
    // `setsid` below does that job and does it in the right order.
    if let Err(e) = rustix::process::setsid() {
        eprintln!("meshware: cannot start a new session for this terminal: {e}");
    }
    // fd 0 is the pts slave the host handed us. Adopting it is what gives
    // the line discipline a process group to signal — the whole reason this
    // shim exists.
    if let Err(e) = rustix::process::ioctl_tiocsctty(std::io::stdin()) {
        eprintln!("meshware: cannot take this terminal as controlling: {e} (Ctrl-C may not work)");
    }
    let err = std::process::Command::new(&program).args(&args).exec();
    // `exec` returns only on failure.
    eprintln!(
        "meshware: cannot run {}: {err}",
        std::path::Path::new(&program).display()
    );
    std::process::exit(127);
}
