//! `<binary> cc <compiler> [args…]` — the wrapper as an argv entry point,
//! so that *any* binary carrying the engine can be `CC`.
//!
//! ## Why this is not just the CLI's subcommand
//!
//! `make` calls `CC` by exec'ing it. There is no way to hand `make` a Rust
//! function, so the wrapper has to be a file on disk that the build can
//! run — and on a machine where somebody installed **Director and nothing
//! else** there is no `meshnode` on `PATH` to be that file. Director embeds
//! its own node (`engine.rs`: "Director *is* a peer"), and its home is
//! deliberately `<meshware home>/director`, separate from any standalone
//! `meshnode` on the same box. So the Build farm panel used to print
//!
//! ```text
//! make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'
//! ```
//!
//! …which on that machine fails twice: `meshnode` is not there, and even
//! when it is, it resolves `~/.meshware`, finds no daemon, and every single
//! compile takes the local-fallback path — silently, because falling back
//! is the wrapper's contract. The build works, feels normal, and
//! distributes nothing.
//!
//! This module is the fix. Director calls [`try_run_argv`] from `main`
//! before it opens a window — the fourth binary-is-also-a-shim in that
//! function, after the PTY host, the Landlock sandbox and
//! `mesh_bench::cli::try_run_argv` — so `CC='<Director> cc gcc'` works with
//! nothing installed at all.
//!
//! ## Hand-parsed, not clap
//!
//! `mesh-core` has no clap, but that is not the reason. The reason is that
//! everything after the wrapper's own flags is *the compiler's* command
//! line, verbatim, hyphens and all, and a parser that gets clever about a
//! `-c` or a `-Wl,--as-needed` corrupts a build in a way that looks like a
//! compiler bug. So: read the flags this wrapper actually owns, stop dead
//! at the first token that is not one of them, and pass the rest through
//! untouched. That is what `trailing_var_arg` + `allow_hyphen_values` buys
//! on the `meshnode cc` side, spelled out.

use super::{Options, run_local};

/// The wrapper's own flags, before the compiler's line begins.
const USAGE: &str = "\
usage: cc [--on SELECTOR] [--verbose] [--local-only] [--timeout SECS] <compiler> [args…]

Compile one translation unit through the mesh, falling back to the local
compiler for anything it cannot distribute. Meant for CC/CXX in a build:

  make -j8 CC='<this binary> cc gcc' CXX='<this binary> cc g++'
";

/// Run as the compiler wrapper when argv says so, else hand back `None`.
///
/// Returns the exit status to leave with — **the compiler's own**, so a
/// build reads it the way it reads any other `cc`.
pub fn try_run_argv() -> Option<i32> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) != Some("cc") {
        return None;
    }
    Some(run_argv(&argv[2..]))
}

/// Split out from [`try_run_argv`] so the parsing has a seam a test can
/// reach without a process to launch.
fn run_argv(args: &[String]) -> i32 {
    let (opts, help) = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("cc: {message}\n\n{USAGE}");
            return 2;
        }
    };
    if help {
        println!("{USAGE}");
        return 0;
    }
    // A runtime per compile, current-thread: this process exists for one
    // translation unit and then dies, and a work-stealing pool's threads
    // would be pure startup cost multiplied by every file in the build.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        // No runtime means no mesh, and no mesh means compile it here —
        // the same answer every other failure in this file gives. This one
        // cannot use `run_local`, which is async: see `fallback_blocking`.
        Err(e) => return fallback_blocking(&opts, &format!("no async runtime ({e})")),
    };
    runtime.block_on(async {
        let submitter = match daemon() {
            Ok(s) => s,
            Err(why) => return fallback(&opts, &why).await,
        };
        match super::run(&submitter, opts.clone()).await {
            Ok(code) => code,
            Err(e) => fallback(&opts, &format!("{e:#}")).await,
        }
    })
}

/// Compile locally, verbatim, and say why if asked.
///
/// Not an error path so much as *the* path, for every unit this wrapper
/// declines: "it never breaks the build" is the contract the whole file is
/// written around, and a wrapper that exited non-zero because a daemon was
/// down would break every build on the machine at once.
///
/// Async, and called with `.await` from inside `block_on`, because
/// [`run_local`] drives a `tokio::process::Child` — building a *second*
/// runtime to host it from inside the first is the one thing tokio panics
/// on outright, and it would panic on exactly the path that exists to never
/// fail.
async fn fallback(opts: &Options, why: &str) -> i32 {
    if opts.verbose {
        eprintln!("cc: local fallback ({why})");
    }
    let Some((compiler, rest)) = opts.argv.split_first() else {
        eprintln!("cc: no compiler to run");
        return 2;
    };
    match run_local(compiler, rest).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cc: {compiler}: {e:#}");
            1
        }
    }
}

/// The same fallback for the one caller that has no runtime to await in:
/// the machine could not give us an event loop. `std::process` rather than
/// tokio's, deliberately — at that point the async stack is the thing that
/// failed, and the build is still owed a compile.
fn fallback_blocking(opts: &Options, why: &str) -> i32 {
    if opts.verbose {
        eprintln!("cc: local fallback ({why})");
    }
    let Some((compiler, rest)) = opts.argv.split_first() else {
        eprintln!("cc: no compiler to run");
        return 2;
    };
    match std::process::Command::new(compiler).args(rest).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cc: cannot run {compiler}: {e}");
            1
        }
    }
}

/// This machine's daemon, as something the wrapper can submit through.
///
/// `MESHWARE_HOME` is what picks *which* node — the caller sets it, and the
/// Build farm panel sets it to Director's own home, which is the whole
/// point of this shim existing.
fn daemon() -> Result<Daemon, String> {
    let home = crate::layout::Layout::resolve(None);
    let socket = crate::config::socket_path_for(home.root());
    if !socket.exists() {
        return Err(format!(
            "no daemon at {} (is the node running?)",
            socket.display()
        ));
    }
    let user = crate::identity::load_cli_user(home.root(), None)
        .map_err(|e| format!("no user identity in {}: {e}", home.root().display()))?;
    Ok(Daemon { socket, user })
}

/// A [`super::Submitter`] that is one Unix socket.
struct Daemon {
    socket: std::path::PathBuf,
    user: crate::identity::Keypair,
}

impl super::Submitter for Daemon {
    fn submit(
        &self,
        req: mesh_proto::IpcRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<tokio::sync::mpsc::Receiver<mesh_proto::IpcResponse>>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let mut stream = crate::ipc::request_stream(&self.socket, &self.user, req).await?;
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move {
                while let Ok(frame) = mesh_proto::read_frame(&mut stream).await {
                    if tx.send(frame).await.is_err() {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }
}

/// Read this wrapper's flags, then stop.
///
/// Returns the options and whether help was asked for. The stopping rule is
/// the load-bearing part: the first token that is not a flag this wrapper
/// owns *is the compiler*, and everything from there on belongs to it.
fn parse(args: &[String]) -> Result<(Options, bool), String> {
    let mut opts = Options {
        argv: Vec::new(),
        on: None,
        verbose: false,
        local_only: false,
        timeout_secs: 600,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Ok((opts, true)),
            "--verbose" | "-v" => {
                opts.verbose = true;
                i += 1;
            }
            "--local-only" => {
                opts.local_only = true;
                i += 1;
            }
            "--on" => {
                opts.on = Some(value(args, i, "--on")?);
                i += 2;
            }
            "--timeout" => {
                let v = value(args, i, "--timeout")?;
                opts.timeout_secs = v
                    .parse()
                    .map_err(|_| format!("--timeout wants seconds, got {v:?}"))?;
                i += 2;
            }
            // `--on=any` and friends, because a Makefile author will write
            // it that way and being right about it costs one branch.
            flag if flag.starts_with("--on=") => {
                opts.on = Some(flag["--on=".len()..].to_string());
                i += 1;
            }
            flag if flag.starts_with("--timeout=") => {
                let v = &flag["--timeout=".len()..];
                opts.timeout_secs = v
                    .parse()
                    .map_err(|_| format!("--timeout wants seconds, got {v:?}"))?;
                i += 1;
            }
            // Not ours. The compiler's line starts here and is not read.
            _ => break,
        }
    }
    opts.argv = args[i..].to_vec();
    if opts.argv.is_empty() {
        return Err("needs a compiler to run".into());
    }
    Ok((opts, false))
}

fn value(args: &[String], i: usize, flag: &str) -> Result<String, String> {
    args.get(i + 1)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    /// The whole reason this is hand-parsed: a compiler line is full of
    /// things that look like flags, and not one of them is ours.
    #[test]
    fn the_compilers_line_is_passed_through_untouched() {
        let (opts, _) = parse(&argv("gcc -c -O2 -Wall -o foo.o foo.c")).expect("parse");
        assert_eq!(opts.argv, argv("gcc -c -O2 -Wall -o foo.o foo.c"));
        assert!(opts.on.is_none());
        assert!(!opts.verbose);
    }

    /// A compiler flag that shares a prefix with one of ours must still
    /// reach the compiler. `--on` is the wrapper's; `--optimize` is not.
    #[test]
    fn a_lookalike_flag_after_the_compiler_is_the_compilers() {
        let (opts, _) = parse(&argv("gcc --on-the-fly -c a.c")).expect("parse");
        assert_eq!(opts.argv, argv("gcc --on-the-fly -c a.c"));
        assert!(opts.on.is_none(), "a compiler flag was eaten as ours");
    }

    #[test]
    fn our_flags_are_read_until_the_compiler_begins() {
        let (opts, _) =
            parse(&argv("--on tag=build --verbose --timeout 90 gcc -c a.c")).expect("parse");
        assert_eq!(opts.on.as_deref(), Some("tag=build"));
        assert!(opts.verbose);
        assert_eq!(opts.timeout_secs, 90);
        assert_eq!(opts.argv, argv("gcc -c a.c"));
    }

    #[test]
    fn the_equals_spelling_works_too() {
        let (opts, _) = parse(&argv("--on=any --timeout=30 clang -c a.c")).expect("parse");
        assert_eq!(opts.on.as_deref(), Some("any"));
        assert_eq!(opts.timeout_secs, 30);
        assert_eq!(opts.argv, argv("clang -c a.c"));
    }

    #[test]
    fn local_only_is_read_and_the_rest_survives() {
        let (opts, _) = parse(&argv("--local-only gcc -c a.c")).expect("parse");
        assert!(opts.local_only);
        assert_eq!(opts.argv, argv("gcc -c a.c"));
    }

    #[test]
    fn a_compiler_is_required() {
        assert!(parse(&argv("--verbose")).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn help_is_help_and_not_a_compiler() {
        let (_, help) = parse(&argv("--help")).expect("parse");
        assert!(help);
    }

    /// The default has to match `meshnode cc --timeout`'s, or the two front
    /// ends time out differently on the same build.
    #[test]
    fn the_timeout_default_matches_the_cli() {
        let (opts, _) = parse(&argv("gcc -c a.c")).expect("parse");
        assert_eq!(opts.timeout_secs, 600);
    }
}
