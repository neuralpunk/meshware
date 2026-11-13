//! `meshnode cc` — a build farm out of the parts already in the engine.
//!
//! ```sh
//! make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'
//! ```
//!
//! This is distcc's idea with ccache's idea folded in, and neither piece is
//! new here: the mesh already places work by capacity, already streams
//! results, and already has a content-addressed action cache that any node
//! can answer from (C7 §11). What was missing was a way to hand it a
//! *compile*, which is a megabyte of preprocessed source and a command line
//! — too big for argv, too transient for the Library. `IpcRequest::RunScript`
//! is that door (`mesh_proto::script`), and this is what walks through it.
//!
//! ## How one compile travels
//!
//! 1. **Preprocess locally.** `cc -E` resolves every `#include` against
//!    *this* machine's headers, so the translation unit that leaves is
//!    self-contained. This is the whole trick, and it is distcc's: it is
//!    what makes a remote compile produce byte-identical output without
//!    the remote machine having the project checked out, the right glibc
//!    headers, or anything else.
//! 2. **Submit it as a script.** The preprocessed text rides inside the job
//!    body, in a heredoc whose delimiter is derived from its own hash, so
//!    no possible source text can terminate it early.
//! 3. **Cache it.** An inline script's action key is the digest of its own
//!    source (C7), and the source contains the preprocessed unit *and* the
//!    compiler flags. So the same translation unit compiled with the same
//!    flags is one key, mesh-wide, forever — which is `ccache`, except the
//!    hit can come off any machine in the house.
//! 4. **Bring the object back.** Base64 on a declared output (M19 §5.2).
//!
//! ## The base64, honestly
//!
//! A `.o` comes back as base64 in the job's declared output, which costs a
//! third more bytes than the file. distcc ships raw. The alternative here
//! is a new object-transfer path for one caller, and 33% of a 200 KB object
//! on a LAN is not worth a protocol; the *inputs* — the big half — travel
//! raw. If a build farm ever justifies a binary return path, this is the
//! call site that will pay for it.
//!
//! ## It never breaks the build
//!
//! That is distcc's real contract and it is the rule here too. Anything
//! this file cannot handle — a link step, a `-E`, an assembler input,
//! multiple sources, no daemon, no mesh, no candidate node, a network
//! failure mid-flight — falls back to running the compiler locally,
//! verbatim, and returns its exit status. The one thing that does **not**
//! fall back is a genuine compile error: the remote compiler's diagnostics
//! are the answer, and recompiling locally to produce the same errors would
//! double the cost of every broken build.

pub mod cli;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use anyhow::{Context as _, Result, bail};
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, Runtime};
use tokio::process::Command;
use tokio::sync::mpsc;

/// How this wrapper names itself in a build log.
///
/// Not the literal string `"meshnode cc"` any more: Director is also a
/// wrapper (`farm::cli`), and a diagnostic that names a binary the user has
/// not got installed sends them looking in the wrong place. Derived from
/// argv[0] so the line in the log points at the thing that printed it.
fn tool() -> String {
    let argv0 = std::env::args().next().unwrap_or_default();
    let name = Path::new(&argv0)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "cc".to_string());
    format!("{name} cc")
}

/// Somewhere to send a job and read its frames back.
///
/// An abstraction rather than the CLI's `Ctx` for one reason: the only
/// question about this file that a unit test cannot answer is *"does a real
/// compiler, on a real node, come back as a real object file"* — and that
/// needs an in-process engine on the other end, not a Unix socket to a
/// daemon that a test would have to install and start. One trait, two
/// implementations, and the wrapper cannot tell them apart.
pub trait Submitter: Sync {
    fn submit(
        &self,
        req: IpcRequest,
    ) -> Pin<Box<dyn Future<Output = Result<mpsc::Receiver<IpcResponse>>> + Send + '_>>;
}

/// Preprocessed units larger than this are compiled locally.
///
/// **Derived from [`mesh_proto::MAX_FRAME_LEN`], not chosen.** The unit
/// travels inside a heredoc, inside a signed job spec, inside one frame —
/// so a ceiling above the frame limit is not a ceiling at all. This was 16
/// MB, and the first real codebase pointed at it (a Linux kernel
/// `allmodconfig`) turned up the consequence: a 5 MB translation unit was
/// preprocessed, submitted, and rejected on the wire, and the build log
/// filled with `local fallback (protocol error: frame too large)`. The
/// answer was right — it compiled locally — but it was paid for with a
/// preprocess and a round trip, and it read like a bug in the mesh rather
/// than a unit that was simply too big.
///
/// The slack is for everything else in the frame: the rest of the script,
/// the spec's ids and signature, the output declaration. A quarter of a
/// megabyte is far more than those need, and the cost of being generous
/// here is one large unit compiling locally that might just have fitted.
const MAX_UNIT: usize = mesh_proto::MAX_FRAME_LEN as usize - 256 * 1024;

/// Assembler directives that name a *file*, which the executor does not
/// have.
///
/// Preprocessing resolves `#include`, and that is what makes a unit
/// self-contained — but it does not touch the contents of an `asm()`
/// statement, and `.incbin "kernel/config_data.gz"` inside one is a path
/// resolved by the assembler, at compile time, relative to the compiler's
/// working directory. Ship that and the executor's assembler says "file not
/// found" — which is a *compile error*, and compile errors deliberately do
/// not fall back (a remote diagnostic is the answer, and rebuilding to
/// reproduce it would double the cost of every broken build). So the
/// wrapper would turn a perfectly good source file into a failed build.
///
/// Found the same way as the ceiling above: the kernel's `kernel/configs.c`
/// does exactly this, and it is the file that took the whole build down.
const FILE_REFERENCING_ASM: [&str; 2] = [".incbin", ".include"];

/// The output key the remote script publishes the object under.
const OBJ_OUTPUT: &str = "obj";

/// What a wrapper invocation turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Distributable: one compile of one source to one object.
    Remote(Compile),
    /// Run it here, unchanged, and say why in `--verbose`.
    Local(&'static str),
}

/// A compile this wrapper understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compile {
    /// The source file, as written on the command line.
    pub source: PathBuf,
    /// Where the object goes.
    pub output: PathBuf,
    /// The language `-x` name for a *preprocessed* input: `cpp-output`,
    /// `c++-cpp-output`, `objective-c-cpp-output`.
    pub language: &'static str,
    /// Flags that survive to the **remote compile**: everything that is not
    /// an include path, a define, a dependency flag, `-c`, `-o` or the
    /// input.
    ///
    /// The exclusions matter more than the inclusions. `-I/usr/local/mine`
    /// names a directory that does not exist on the executor, and
    /// `-include config.h` would include a header *into an already
    /// preprocessed unit* — a second copy of everything it declares. The
    /// preprocessing already happened; what reaches the compiler is code
    /// generation and nothing else.
    pub compile_flags: Vec<String>,
}

/// Source extensions worth distributing, and the `-x` name their
/// preprocessed form takes.
fn language_of(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "c" => Some("cpp-output"),
        "cc" | "cp" | "cxx" | "cpp" | "CPP" | "c++" | "C" => Some("c++-cpp-output"),
        "m" => Some("objective-c-cpp-output"),
        "mm" | "M" => Some("objective-c++-cpp-output"),
        _ => None,
    }
}

/// Flags that take a separate argument, so the argument is not mistaken for
/// a source file.
///
/// Missing one here is the bug that matters: `-o` unlisted would make
/// `foo.o` look like an input, and the wrapper would distribute nonsense.
/// So the list is long and the default for an unknown `-` flag is to keep
/// it and assume it takes no argument, which is right for the overwhelming
/// majority and wrong safely (a stray argument that looks like a source
/// file lands us in [`Plan::Local`] via the "more than one input" rule).
const TAKES_ARG: &[&str] = &[
    "-o",
    "-I",
    "-i",
    "-D",
    "-U",
    "-L",
    "-l",
    "-include",
    "-imacros",
    "-idirafter",
    "-iprefix",
    "-iwithprefix",
    "-iwithprefixbefore",
    "-isystem",
    "-isysroot",
    "-iquote",
    "-imultilib",
    "-MF",
    "-MT",
    "-MQ",
    "-x",
    "-Xpreprocessor",
    "-Xassembler",
    "-Xlinker",
    "-aux-info",
    "--param",
    "-B",
    "-specs",
    "-T",
    "-u",
    "-z",
];

/// Flags that belong only to the preprocessing half, and so must **not**
/// reach the remote compile.
///
/// This is the list that has to be right. A missing entry sends an include
/// path or a header injection to a machine that has neither, against an
/// input that has already been preprocessed.
fn is_preprocess_only(arg: &str) -> bool {
    arg.starts_with("-I")
        || arg.starts_with("-D")
        || arg.starts_with("-U")
        || arg.starts_with("-isystem")
        || arg.starts_with("-iquote")
        || arg.starts_with("-idirafter")
        || arg.starts_with("-iprefix")
        || arg.starts_with("-iwithprefix")
        || arg.starts_with("-include")
        || arg.starts_with("-imacros")
        || arg.starts_with("-isysroot")
        || arg.starts_with("--sysroot")
        || arg.starts_with("-nostdinc")
        || arg.starts_with("-M")
        // `-Wp,…` and `-Xpreprocessor` pass their payload straight to cpp.
        // Forwarding one to the remote compile would run a preprocessor
        // directive against an already-preprocessed unit — most often
        // `-Wp,-MD,foo.d`, which would write a dependency file on the
        // executor and none where make is looking.
        || arg.starts_with("-Wp,")
        || arg == "-Xpreprocessor"
}

/// Decide what to do with a compiler command line.
///
/// The bar for [`Plan::Remote`] is deliberately high. Everything this does
/// not certainly understand compiles locally, because a wrapper that is
/// clever about an edge case it half-understands produces a build that is
/// subtly wrong, and there is no worse failure mode for a build tool.
pub fn plan(args: &[String]) -> Plan {
    let mut compiling = false;
    let mut output: Option<PathBuf> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut explicit_language: Option<String> = None;
    let mut compile_flags = Vec::new();
    // `-MD`/`-MMD` write a dependency file as a side effect, and gcc derives
    // its name from `-o` — which the preprocessor pass does not get. Seen
    // together with an explicit `-MF` that is fine (the name is stated);
    // seen alone, the `.d` would land somewhere `make` is not looking, and
    // a build with silently misplaced dependency files rebuilds the wrong
    // things forever. cmake, ninja and meson all pass `-MF`.
    let mut implicit_deps = false;
    let mut named_dep_file = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-c" => {
                compiling = true;
                i += 1;
                continue;
            }
            // Already a preprocessor or assembler run: there is nothing to
            // distribute, and `-E` in particular is what *we* are about to
            // do — running it remotely would resolve includes against the
            // wrong machine's headers, which is the one way to be wrong
            // and still produce output.
            "-E" | "-M" | "-MM" | "-S" => return Plan::Local("preprocess/assemble only"),
            // Whole-program modes read files the remote machine does not
            // have and write files it cannot put back.
            "-flto" | "-fwhole-program" | "--coverage" | "-fprofile-generate" | "-fprofile-use"
            | "-save-temps" => {
                return Plan::Local("a flag that needs the whole build tree");
            }
            // Precompiled headers are a *binary* the executor does not have
            // and could not use if it did — they are tied to the exact
            // compiler build that made them.
            "-include-pch" | "-Winvalid-pch" | "-fpch-preprocess" => {
                return Plan::Local("a precompiled header");
            }
            "-o" => {
                let Some(next) = args.get(i + 1) else {
                    return Plan::Local("-o with no argument");
                };
                output = Some(PathBuf::from(next));
                i += 2;
                continue;
            }
            "-x" => {
                let Some(next) = args.get(i + 1) else {
                    return Plan::Local("-x with no argument");
                };
                // Recorded, not forwarded to the compile: the remote input
                // is *preprocessed*, so its `-x` is the `-cpp-output`
                // variant this wrapper supplies, and the original would
                // override it and hand the compiler the wrong language.
                // The preprocessor still sees it — it sees everything.
                explicit_language = Some(next.clone());
                i += 2;
                continue;
            }
            _ => {}
        }
        if let Some(rest) = arg.strip_prefix("-o")
            && !rest.is_empty()
            && arg.len() > 2
        {
            output = Some(PathBuf::from(rest));
            i += 1;
            continue;
        }
        // Response files and `-` (stdin) both hide the real command line.
        if arg.starts_with('@') || arg == "-" {
            return Plan::Local("a response file or stdin input");
        }
        // Anything that is a coverage/profile/debug-prefix flag naming a
        // path is a flag whose *output* location matters on the machine
        // that runs it, and that is not this one.
        if arg.starts_with("-fprofile") || arg.starts_with("-fdebug-prefix-map=") {
            return Plan::Local("a flag that writes beside the object");
        }
        if arg == "-MD" || arg == "-MMD" {
            implicit_deps = true;
        }
        if arg == "-MF" || arg.starts_with("-MF") && arg.len() > 3 {
            named_dep_file = true;
        }
        if arg.starts_with('-') {
            let takes = TAKES_ARG.contains(&arg.as_str());
            // Only the compile half is classified. The preprocessor gets
            // the whole original command line (see `preprocess_argv`), so a
            // flag this list has never heard of cannot change what the unit
            // contains — it can only fail to reach a compiler that would
            // have ignored it.
            if !is_preprocess_only(arg) {
                compile_flags.push(arg.clone());
            }
            if takes {
                let Some(next) = args.get(i + 1) else {
                    return Plan::Local("a flag with no argument");
                };
                if !is_preprocess_only(arg) {
                    compile_flags.push(next.clone());
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        inputs.push(PathBuf::from(arg));
        i += 1;
    }

    if !compiling {
        // No `-c` means a link, and a link needs every object and every
        // library on the machine that runs it.
        return Plan::Local("a link step");
    }
    if implicit_deps && !named_dep_file {
        return Plan::Local("-MD/-MMD without -MF");
    }
    if inputs.len() != 1 {
        return Plan::Local("not exactly one input file");
    }
    if let Some(x) = &explicit_language
        && !matches!(x.as_str(), "c" | "c++" | "objective-c" | "objective-c++")
    {
        return Plan::Local("an explicit -x language this wrapper does not map");
    }
    let source = inputs.remove(0);
    let Some(language) = language_of(&source) else {
        return Plan::Local("a source extension this wrapper does not map");
    };
    let output = match output {
        Some(o) => o,
        // `cc -c foo.c` writes `foo.o` in the *current* directory, not
        // beside the source. Getting this wrong would put objects in the
        // wrong place, silently, on every build that omits `-o`.
        None => PathBuf::from(format!(
            "{}.o",
            source
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "a".into())
        )),
    };
    Plan::Remote(Compile {
        source,
        output,
        language,
        compile_flags,
    })
}

// ---------------------------------------------------------------------------
// Running one compile
// ---------------------------------------------------------------------------

/// How `meshnode cc` was invoked.
#[derive(Debug, Clone)]
pub struct Options {
    /// The compiler and its arguments, as `make` wrote them.
    pub argv: Vec<String>,
    /// Selector for where compiles land.
    pub on: Option<String>,
    /// Say what happened on stderr — which node, hit or miss, or why it
    /// stayed local.
    pub verbose: bool,
    /// Never distribute; only use the cache. The honest thing to try when
    /// a build misbehaves under `--on`.
    pub local_only: bool,
    /// Per-compile timeout.
    pub timeout_secs: u64,
}

/// Where one object actually came from.
///
/// Returned rather than only logged, because "it worked" and "it worked
/// *through the mesh*" are different claims and a build farm that silently
/// falls back to local compiles on every unit would pass every test that
/// only checked the first one. This is what lets a test tell them apart —
/// and what `--verbose` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// Compiled on this node's mesh, by the named executor.
    Mesh { node: String },
    /// Served from the action cache — nobody compiled anything.
    Cached,
    /// Compiled here, with the reason.
    Local(String),
}

impl How {
    pub fn describe(&self) -> String {
        match self {
            How::Mesh { node } => node.clone(),
            How::Cached => "cache".to_string(),
            How::Local(why) => format!("local ({why})"),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, How::Local(_))
    }
}

/// What one `meshnode cc` invocation did.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The status the caller should exit with.
    pub code: i32,
    pub how: How,
}

/// Run the wrapper. Returns the exit status the caller should exit with.
pub async fn run(ctx: &dyn Submitter, opts: Options) -> Result<i32> {
    Ok(run_reported(ctx, opts).await?.code)
}

/// [`run`], and say where the object came from.
pub async fn run_reported(ctx: &dyn Submitter, opts: Options) -> Result<Outcome> {
    let Some((compiler, args)) = opts.argv.split_first() else {
        bail!("`meshnode cc` needs a compiler: `meshnode cc gcc -c foo.c -o foo.o`");
    };
    let plan = if opts.local_only {
        Plan::Local("--local-only")
    } else {
        plan(args)
    };
    let compile = match plan {
        Plan::Remote(c) => c,
        Plan::Local(why) => {
            if opts.verbose {
                eprintln!("{}: local ({why})", tool());
            }
            return Ok(Outcome {
                code: local(compiler, args).await?,
                how: How::Local(why.to_string()),
            });
        }
    };

    match distribute(ctx, &opts, compiler, args, &compile).await {
        Ok(Distributed::Object { bytes, where_, hit }) => {
            if let Some(parent) = compile.output.parent()
                && !parent.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&compile.output, &bytes)
                .with_context(|| format!("writing {}", compile.output.display()))?;
            let how = if hit {
                How::Cached
            } else {
                How::Mesh { node: where_ }
            };
            if opts.verbose {
                eprintln!(
                    "{}: {} <- {} ({} bytes)",
                    tool(),
                    compile.output.display(),
                    how.describe(),
                    bytes.len()
                );
            }
            Ok(Outcome { code: 0, how })
        }
        // The remote compiler ran and disliked the code. Its diagnostics
        // *are* the answer; recompiling locally would print the same
        // errors and double the cost of every broken build.
        Ok(Distributed::CompileError { code, stderr }) => {
            eprint!("{stderr}");
            Ok(Outcome {
                code,
                how: How::Mesh {
                    node: "the mesh".into(),
                },
            })
        }
        Err(e) => {
            // Everything else is infrastructure, and infrastructure must
            // never be the reason a build fails.
            if opts.verbose {
                eprintln!("{}: local fallback ({e})", tool());
            }
            Ok(Outcome {
                code: local(compiler, args).await?,
                how: How::Local(e.to_string()),
            })
        }
    }
}

enum Distributed {
    Object {
        bytes: Vec<u8>,
        where_: String,
        hit: bool,
    },
    CompileError {
        code: i32,
        stderr: String,
    },
}

async fn distribute(
    ctx: &dyn Submitter,
    opts: &Options,
    compiler: &str,
    args: &[String],
    compile: &Compile,
) -> Result<Distributed> {
    let unit = preprocess(compiler, args, compile).await?;
    if unit.len() > MAX_UNIT {
        bail!(
            "preprocessed unit is {} MB, over the {} MB a job frame can carry",
            unit.len() / (1024 * 1024),
            MAX_UNIT / (1024 * 1024)
        );
    }
    // Only knowable after preprocessing: `plan` sees the command line, and
    // this is in the source.
    if let Some(directive) = file_referencing_asm(&unit) {
        bail!("inline asm uses {directive}, which names a file the executor has not got");
    }
    let script = remote_script(compiler, compile, &unit);
    let mut env = vec![
        // Cacheable, and this is the claim the whole thing rests on: a
        // preprocessed translation unit plus a compiler command line is a
        // pure function, which is exactly the claim `--cacheable` asks the
        // submitter to make (C7 §11.2). It is *true* here in a way it
        // usually is not — the preprocessing already happened, so there is
        // no header on the executor that can change the answer.
        (mesh_proto::REQ_CACHE.to_string(), "1".to_string()),
    ];
    env.extend(crate::outputs::Wanted::keys([OBJ_OUTPUT]).to_env_with(None));

    let mut stream = ctx
        .submit(IpcRequest::RunScript {
            script: Box::new(mesh_proto::content::InlineScript {
                runtime: Runtime::Bash,
                source: script,
                requirements: String::new(),
                inputs: Vec::new(),
            }),
            on: opts.on.clone(),
            env,
            timeout_secs: Some(opts.timeout_secs),
            priority: JobPriority::Normal,
        })
        .await?;

    let mut where_ = String::from("mesh");
    let mut hit = false;
    let mut stderr = String::new();
    let mut encoded: Option<String> = None;
    let mut exit_code: Option<i32> = None;
    while let Some(frame) = stream.recv().await {
        match frame {
            IpcResponse::CacheHit(_) => hit = true,
            IpcResponse::JobStarted { executor_name, .. } => where_ = executor_name,
            IpcResponse::LogChunk { stream: 2, data } => {
                stderr.push_str(&String::from_utf8_lossy(&data));
            }
            IpcResponse::Result(row) => {
                exit_code = row.exit_code;
                encoded = row.output(OBJ_OUTPUT).map(str::to_string);
            }
            IpcResponse::JobDone { exit_code: c, .. } => {
                exit_code = exit_code.or(c);
                break;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                bail!("{message}")
            }
            _ => {}
        }
    }

    match exit_code {
        Some(0) => {}
        // The compiler spoke. Pass its verdict straight through.
        Some(code) => {
            return Ok(Distributed::CompileError {
                code,
                stderr: strip_marker(&stderr),
            });
        }
        None => bail!("the compile job ended without an exit status"),
    }

    let Some(encoded) = encoded else {
        bail!("the compile succeeded but published no object");
    };
    // A large object spills to an M9 ephemeral object and the row carries a
    // reference instead of the bytes (`mesh_core::outputs`).
    let encoded = match crate::outputs::parse_object_ref(&encoded) {
        Some((id, _)) => fetch_object(ctx, id).await?,
        None => encoded,
    };
    let bytes = base64_decode(encoded.trim())
        .ok_or_else(|| anyhow::anyhow!("the published object is not valid base64"))?;
    if bytes.is_empty() {
        bail!("the compile published an empty object");
    }
    // Compiler warnings reach the developer either way — they are the
    // point of `-Wall`, and swallowing them because the compile happened
    // elsewhere would be the first thing anybody noticed.
    if !stderr.trim().is_empty() {
        eprint!("{}", strip_marker(&stderr));
    }
    Ok(Distributed::Object { bytes, where_, hit })
}

/// Drop the output marker line from what is shown to a developer: it is
/// the mesh's plumbing, not the compiler's opinion.
/// The first [`FILE_REFERENCING_ASM`] directive in a preprocessed unit, if
/// any.
///
/// A plain substring scan, and deliberately not a parse. Being wrong in the
/// cautious direction costs one unit compiled locally; being wrong the other
/// way fails the build. A comment mentioning `.incbin` is enough to keep a
/// unit at home, and that is the correct trade.
fn file_referencing_asm(unit: &str) -> Option<&'static str> {
    FILE_REFERENCING_ASM
        .iter()
        .find(|d| unit.contains(**d))
        .copied()
}

fn strip_marker(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.trim_start().starts_with(crate::outputs::OUTPUT_MARKER))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// The preprocessor's command line: the original one, minus the three
/// things that describe the *compile*, plus `-E`.
///
/// Deliberately not "the flags this wrapper classified as
/// preprocessor-ish". Reclassifying a command line and hoping the subset is
/// complete is how a wrapper silently changes what a translation unit
/// contains; the preprocessor is perfectly happy to be handed `-O2` and
/// ignore it, so the safe rule is to hand it **everything** and take away
/// only what would confuse it:
///
/// - `-c`, which `-E` overrides anyway but which reads as a contradiction;
/// - `-o <file>`, because the unit comes back on stdout and passing the
///   object's path here would truncate the object with preprocessed text;
/// - the source, which is re-added last so it is unambiguously the input.
pub fn preprocess_argv(args: &[String], compile: &Compile) -> Vec<String> {
    let mut out = vec!["-E".to_string()];
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-c" {
            continue;
        }
        if arg == "-o" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("-o") && arg.len() > 2 {
            continue;
        }
        if !arg.starts_with('-') && Path::new(arg) == compile.source {
            continue;
        }
        out.push(arg.clone());
    }
    out.push(compile.source.to_string_lossy().into_owned());
    out
}

/// Run the preprocessor locally and capture the translation unit.
async fn preprocess(compiler: &str, args: &[String], compile: &Compile) -> Result<String> {
    let mut cmd = Command::new(compiler);
    cmd.args(preprocess_argv(args, compile));
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd
        .output()
        .await
        .with_context(|| format!("running {compiler} -E"))?;
    if !out.status.success() {
        // A preprocessor failure is a real error in the source (a missing
        // header, usually) and the local compiler will produce exactly the
        // same one — so this falls back rather than reporting, and the
        // developer sees one copy of the diagnostics instead of two.
        bail!(
            "preprocessing failed locally: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("the preprocessed unit is not UTF-8")
}

/// The bash the executor runs.
///
/// The unit goes in a **quoted** heredoc, so nothing in it is expanded, and
/// the delimiter carries a digest of the unit itself — no source text can
/// contain a line that ends its own heredoc, because that would require
/// predicting its own hash.
fn remote_script(compiler: &str, compile: &Compile, unit: &str) -> String {
    let tag = format!("MW_TU_{}", &blake3::hash(unit.as_bytes()).to_hex()[..16]);
    let ext = match compile.language {
        "c++-cpp-output" => "ii",
        "objective-c-cpp-output" => "mi",
        "objective-c++-cpp-output" => "mii",
        _ => "i",
    };
    let flags: String = compile
        .compile_flags
        .iter()
        .map(|f| shell_quote(f))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "set -eu\n\
         cat > unit.{ext} <<'{tag}'\n\
         {unit}\n\
         {tag}\n\
         {cc} -x {lang} -c unit.{ext} -o out.o {flags}\n\
         # The object comes back as base64 on a declared output (M19 §5.2);\n\
         # above 64 KiB the value spills to an ephemeral object and only a\n\
         # reference is recorded, which the wrapper then fetches.\n\
         #\n\
         # `-w0` is GNU coreutils; busybox base64 has no `-w` at all and a\n\
         # node built on it would fail every compile it was given (falling\n\
         # back locally each time, so the build works and the farm quietly\n\
         # does nothing). `tr` is the portable spelling of the same thing.\n\
         encoded=$(base64 -w0 out.o 2>/dev/null || base64 out.o | tr -d '\\n')\n\
         printf '{marker} {key}=%s\\n' \"$encoded\"\n",
        cc = shell_quote(compiler),
        lang = compile.language,
        marker = crate::outputs::OUTPUT_MARKER,
        key = OBJ_OUTPUT,
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Pull an object's bytes back through the daemon.
async fn fetch_object(ctx: &dyn Submitter, id: mesh_proto::ObjectId) -> Result<String> {
    let mut stream = ctx
        .submit(IpcRequest::ObjectGet {
            id,
            hints: Vec::new(),
        })
        .await?;
    let mut buf = Vec::new();
    while let Some(frame) = stream.recv().await {
        match frame {
            IpcResponse::ObjectChunk { data } => buf.extend_from_slice(&data),
            IpcResponse::ObjectEnd { .. } => break,
            IpcResponse::Error { message } => bail!("fetching the object: {message}"),
            _ => {}
        }
    }
    String::from_utf8(buf).context("the fetched object is not base64 text")
}

/// Run the compiler here, unchanged.
///
/// Public because the fallback rule starts before the mesh does: a
/// `meshnode cc` on a machine with no user identity at all must still
/// compile, and that decision is made in `main` before a `Ctx` exists.
pub async fn run_local(compiler: &str, args: &[String]) -> Result<i32> {
    local(compiler, args).await
}

async fn local(compiler: &str, args: &[String]) -> Result<i32> {
    let status = Command::new(compiler)
        .args(args)
        .status()
        .await
        .with_context(|| format!("running {compiler}"))?;
    // A compiler killed by a signal has no exit code; 128+signo is the
    // shell's convention and what `make` expects to see.
    Ok(status.code().unwrap_or(128 + 9))
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

/// Decode standard base64.
///
/// Thirty lines rather than a dependency: this decodes exactly one thing,
/// produced by `base64 -w0` a few lines above, and the whole of the format
/// that matters is here to read.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 255;
    let mut table = [INVALID; 256];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    while i < alphabet.len() {
        table[alphabet[i] as usize] = i as u8;
        i += 1;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes() {
        if b == b'=' {
            break;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        let v = table[b as usize];
        if v == INVALID {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling has to be under the frame that carries it. It was not —
    /// 16 MB against a 4 MB frame — and every kernel translation unit over
    /// 4 MB paid a preprocess and a round trip to find out, then reported a
    /// protocol error into the build log on its way to compiling locally.
    #[test]
    fn a_unit_that_fits_the_ceiling_fits_a_frame() {
        assert!(
            MAX_UNIT < mesh_proto::MAX_FRAME_LEN as usize,
            "a unit at the ceiling cannot be sent"
        );
        // And with room left for the script and the spec around it.
        assert!(mesh_proto::MAX_FRAME_LEN as usize - MAX_UNIT >= 128 * 1024);
    }

    /// `kernel/configs.c`, near enough: the assembler is told to swallow a
    /// file that exists here and nowhere else. Preprocessing does not touch
    /// the inside of an `asm()`, so nothing before this notices.
    #[test]
    fn inline_asm_that_names_a_file_stays_local() {
        let unit = r#"
            static const char kernel_config_data[] __attribute__((used)) =
              "\t.incbin \"kernel/config_data.gz\"\t\n";
        "#;
        assert_eq!(file_referencing_asm(unit), Some(".incbin"));
        assert_eq!(
            file_referencing_asm("asm(\".include \\\"x.s\\\"\");"),
            Some(".include")
        );
    }

    #[test]
    fn ordinary_code_is_not_mistaken_for_it() {
        assert_eq!(file_referencing_asm("int main(void) { return 0; }"), None);
        // The words alone, without the leading dot, are not the directive.
        assert_eq!(file_referencing_asm("// incbin and include are fine"), None);
    }

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn remote(s: &str) -> Compile {
        match plan(&argv(s)) {
            Plan::Remote(c) => c,
            Plan::Local(why) => panic!("{s:?} stayed local: {why}"),
        }
    }

    fn why_local(s: &str) -> &'static str {
        match plan(&argv(s)) {
            Plan::Local(why) => why,
            Plan::Remote(c) => panic!("{s:?} was distributed as {c:?}"),
        }
    }

    #[test]
    fn the_ordinary_compile_is_distributed() {
        let c = remote("-c foo.c -o foo.o");
        assert_eq!(c.source, Path::new("foo.c"));
        assert_eq!(c.output, Path::new("foo.o"));
        assert_eq!(c.language, "cpp-output");
    }

    #[test]
    fn the_default_object_name_is_the_one_cc_would_use() {
        // `cc -c src/foo.c` writes `foo.o` in the *current* directory, not
        // `src/foo.o`. Getting this wrong puts objects in the wrong place
        // on every build that omits `-o`.
        let c = remote("-c src/foo.c");
        assert_eq!(c.output, Path::new("foo.o"));
    }

    #[test]
    fn cpp_extensions_map_to_the_cpp_preprocessed_language() {
        for (src, lang) in [
            ("a.c", "cpp-output"),
            ("a.cc", "c++-cpp-output"),
            ("a.cpp", "c++-cpp-output"),
            ("a.cxx", "c++-cpp-output"),
            ("a.m", "objective-c-cpp-output"),
        ] {
            assert_eq!(remote(&format!("-c {src}")).language, lang, "{src}");
        }
    }

    #[test]
    fn the_preprocessor_gets_the_whole_command_line() {
        // The safe rule, and the reason there is no second classification
        // to keep in step: the preprocessor is handed everything except the
        // three things that describe the compile.
        let args = argv("-c foo.c -o foo.o -I include -DNDEBUG -O2 -Wall -std=c11");
        let c = remote("-c foo.c -o foo.o -I include -DNDEBUG -O2 -Wall -std=c11");
        let pre = preprocess_argv(&args, &c);
        assert_eq!(pre[0], "-E");
        for flag in ["-I", "include", "-DNDEBUG", "-O2", "-Wall", "-std=c11"] {
            assert!(
                pre.contains(&flag.to_string()),
                "{flag} missing from {pre:?}"
            );
        }
        assert!(!pre.contains(&"-c".to_string()), "-c contradicts -E");
        assert!(
            !pre.contains(&"foo.o".to_string()),
            "-o would truncate the object with preprocessed text"
        );
        assert_eq!(pre.last().map(String::as_str), Some("foo.c"), "input last");
        assert_eq!(
            pre.iter().filter(|a| *a == "foo.c").count(),
            1,
            "the source appears exactly once"
        );
    }

    #[test]
    fn a_joined_output_flag_is_also_kept_from_the_preprocessor() {
        let args = argv("-c foo.c -ofoo.o");
        let c = remote("-c foo.c -ofoo.o");
        let pre = preprocess_argv(&args, &c);
        assert!(!pre.iter().any(|a| a.starts_with("-o")), "{pre:?}");
    }

    #[test]
    fn preprocessor_flags_do_not_reach_the_remote_compile() {
        // The half that has to be right: `-I` names a directory the
        // executor does not have, and `-include config.h` would inject a
        // header into an already-preprocessed unit — a second copy of
        // everything it declares.
        let c = remote("-c foo.c -o foo.o -I include -DNDEBUG -include config.h -O2 -Wall");
        assert!(c.compile_flags.contains(&"-O2".to_string()));
        assert!(c.compile_flags.contains(&"-Wall".to_string()));
        for flag in ["-I", "include", "-DNDEBUG", "-include", "config.h"] {
            assert!(
                !c.compile_flags.contains(&flag.to_string()),
                "{flag} reached the compile: {:?}",
                c.compile_flags
            );
        }
    }

    #[test]
    fn a_standard_flag_reaches_the_compile_and_the_preprocessor() {
        // `-std=c11` changes which macros the preprocessor defines *and*
        // what the compiler accepts, so it has to survive both halves.
        let c = remote("-c foo.c -o foo.o -std=c11");
        assert!(c.compile_flags.contains(&"-std=c11".to_string()));
        let args = argv("-c foo.c -o foo.o -std=c11");
        assert!(preprocess_argv(&args, &c).contains(&"-std=c11".to_string()));
    }

    #[test]
    fn an_explicit_language_does_not_override_the_preprocessed_one() {
        // The remote input is `.i`, not `.c`. Forwarding the original `-x c`
        // would tell the compiler to preprocess it a second time.
        let c = remote("-x c -c foo.c -o foo.o");
        assert!(!c.compile_flags.contains(&"-x".to_string()));
        assert_eq!(c.language, "cpp-output");
    }

    #[test]
    fn preprocessor_pass_through_flags_stay_off_the_remote_compile() {
        // `-Wp,-MD,foo.d` on the executor writes a dependency file where
        // make is not looking; `-Xpreprocessor` is the same hazard spelled
        // differently.
        let c = remote("-c foo.c -o foo.o -Wp,-MD,foo.d -O2");
        assert!(!c.compile_flags.iter().any(|f| f.starts_with("-Wp,")));
        assert!(c.compile_flags.contains(&"-O2".to_string()));
        // …and a sysroot names a tree the executor does not have.
        let c = remote("-c foo.c -o foo.o --sysroot=/opt/cross");
        assert!(!c.compile_flags.iter().any(|f| f.starts_with("--sysroot")));
    }

    #[test]
    fn a_precompiled_header_stays_local() {
        assert_eq!(
            why_local("-c foo.c -o foo.o -include-pch all.h.gch"),
            "a precompiled header"
        );
    }

    #[test]
    fn the_remote_script_encodes_the_object_portably() {
        // busybox `base64` has no `-w`, and a node built on it would fail
        // every compile it was handed — silently, because the wrapper falls
        // back locally each time and the build still works.
        let c = remote("-c foo.c -o foo.o");
        let script = remote_script("gcc", &c, "int x;");
        assert!(script.contains("base64 -w0 out.o 2>/dev/null"), "{script}");
        assert!(script.contains("tr -d"), "{script}");
    }

    #[test]
    fn implicit_dependency_files_stay_local() {
        // `-MD` derives the `.d` name from `-o`, which the preprocessor
        // pass does not get — so the file would land where `make` is not
        // looking, and a build with misplaced dependency files rebuilds the
        // wrong things forever.
        assert_eq!(why_local("-c foo.c -o foo.o -MD"), "-MD/-MMD without -MF");
        assert_eq!(why_local("-c foo.c -o foo.o -MMD"), "-MD/-MMD without -MF");
        // Named explicitly, it is fine — and this is what cmake, ninja and
        // meson all emit.
        assert!(matches!(
            plan(&argv("-c foo.c -o foo.o -MD -MF foo.d")),
            Plan::Remote(_)
        ));
    }

    #[test]
    fn joined_output_flags_parse() {
        assert_eq!(remote("-c foo.c -ofoo.o").output, Path::new("foo.o"));
    }

    #[test]
    fn a_link_step_stays_local() {
        assert_eq!(why_local("foo.o bar.o -o prog"), "a link step");
        assert_eq!(why_local("-o prog foo.c"), "a link step");
    }

    #[test]
    fn everything_uncertain_stays_local() {
        assert_eq!(why_local("-E foo.c"), "preprocess/assemble only");
        assert_eq!(why_local("-c -S foo.c"), "preprocess/assemble only");
        assert_eq!(why_local("-c -M foo.c"), "preprocess/assemble only");
        assert_eq!(why_local("-c a.c b.c"), "not exactly one input file");
        assert_eq!(
            why_local("-c foo.s -o foo.o"),
            "a source extension this wrapper does not map"
        );
        assert_eq!(why_local("-c @args"), "a response file or stdin input");
        assert_eq!(
            why_local("-c -flto foo.c"),
            "a flag that needs the whole build tree"
        );
        assert_eq!(
            why_local("-c --coverage foo.c"),
            "a flag that needs the whole build tree"
        );
        assert_eq!(
            why_local("-c -fprofile-arcs foo.c"),
            "a flag that writes beside the object"
        );
        assert_eq!(why_local("-c foo.c -o"), "-o with no argument");
    }

    #[test]
    fn an_unmapped_explicit_language_stays_local() {
        assert_eq!(
            why_local("-x assembler -c foo.c -o foo.o"),
            "an explicit -x language this wrapper does not map"
        );
        // …but a mapped one is fine.
        assert!(matches!(
            plan(&argv("-x c -c foo.c -o foo.o")),
            Plan::Remote(_)
        ));
    }

    #[test]
    fn the_heredoc_delimiter_cannot_appear_in_the_unit() {
        // The one way a heredoc can end early is if the source contains the
        // delimiter. Deriving it from the unit's own hash means source text
        // would have to predict its own digest.
        let compile = remote("-c foo.c -o foo.o");
        let unit = "int main(void){return 0;}";
        let script = remote_script("gcc", &compile, unit);
        let tag = script
            .lines()
            .find_map(|l| l.strip_prefix("cat > unit.i <<'"))
            .and_then(|l| l.strip_suffix('\''))
            .expect("a heredoc tag");
        assert!(tag.starts_with("MW_TU_"));
        assert!(!unit.contains(tag));
        assert!(script.contains(unit));
        assert!(script.contains("base64 -w0 out.o"));
        assert!(script.contains("-x cpp-output"));
    }

    #[test]
    fn identical_units_and_flags_produce_an_identical_script() {
        // Which is the whole cache story: an inline script's action key is
        // the digest of its source (C7), so two machines compiling the same
        // translation unit the same way derive one key.
        let c = remote("-c foo.c -o foo.o -O2");
        let a = remote_script("gcc", &c, "int x;");
        let b = remote_script("gcc", &c, "int x;");
        assert_eq!(a, b);
        // …and a different flag is a different action.
        let d = remote("-c foo.c -o foo.o -O0");
        assert_ne!(a, remote_script("gcc", &d, "int x;"));
        // …as is a different unit.
        assert_ne!(a, remote_script("gcc", &c, "int y;"));
        // …as is a different compiler.
        assert_ne!(a, remote_script("clang", &c, "int x;"));
    }

    #[test]
    fn compile_flags_are_quoted_before_they_cross_a_shell() {
        let c = remote("-c foo.c -o foo.o");
        let mut c = c;
        c.compile_flags.push("-DMSG=\"hi there\"".to_string());
        let script = remote_script("gcc", &c, "int x;");
        assert!(script.contains(r#"'-DMSG="hi there"'"#), "{script}");
    }

    #[test]
    fn base64_round_trips_what_the_remote_writes() {
        // Vectors from RFC 4648 §10, plus the padding-free form `base64
        // -w0` never produces but a trimmed value might.
        for (text, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(
                base64_decode(encoded).as_deref(),
                Some(text.as_bytes()),
                "{encoded}"
            );
        }
        // Binary, which is what an object file actually is.
        let bytes: Vec<u8> = (0..=255u8).collect();
        let encoded = encode_for_test(&bytes);
        assert_eq!(base64_decode(&encoded), Some(bytes));
        // Newlines from a wrapped encoder are tolerated.
        assert_eq!(base64_decode("Zm9v\nYmFy"), Some(b"foobar".to_vec()));
        // Nonsense is refused rather than half-decoded.
        assert_eq!(base64_decode("not base64!"), None);
    }

    fn encode_for_test(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn the_output_marker_is_kept_out_of_the_developers_stderr() {
        let noisy = format!(
            "foo.c:1:1: warning: something\n{} obj=AAAA\nfoo.c:2: note: here\n",
            crate::outputs::OUTPUT_MARKER
        );
        let clean = strip_marker(&noisy);
        assert!(clean.contains("warning: something"));
        assert!(clean.contains("note: here"));
        assert!(!clean.contains("meshware-output"));
    }
}
