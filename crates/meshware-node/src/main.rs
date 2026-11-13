//! Meshware Node — `meshnode`: the mesh daemon and CLI in one binary.
//!
//! Grammar (MASTERPLAN §13): consistent `noun verb`, `--output json|table`
//! on reads, zero-config `meshnode up`.

/// The allocator the shipped binary uses (ENG-25).
///
/// **Measured, not assumed.** Releases are static musl and musl's
/// allocator serialises under thread contention, so every published
/// performance number — taken from a glibc test build — described
/// something nobody runs. Measured on a 16-core machine, 100 nodes:
///
/// | build              | 100 nodes up | submit→complete |
/// |--------------------|-------------:|----------------:|
/// | glibc              |        9.0 s |           76 ms |
/// | musl (as shipped)  |       12.6 s |           72 ms |
/// | musl + mimalloc    |        9.5 s |           25 ms |
///
/// Two separate findings. musl's 40% penalty bringing a hundred nodes up
/// is the contention the item suspected, and mimalloc removes it. The
/// three-fold drop in the dispatch round trip is *not* musl-specific —
/// glibc + mimalloc measures the same 25 ms — so it is an allocator win
/// on the path MASTERPLAN bet #3 calls the product, and it applies to
/// every node whatever it was built against.
///
/// It is C, which `deny.toml` is otherwise firm about keeping out of this
/// binary; the note there records why this one is in. It links statically
/// and adds no runtime dependency.
///
/// Here rather than in `mesh-core`: a `#[global_allocator]` belongs to the
/// binary that is linked, and putting one in the library would choose the
/// allocator for every test binary and for Director, neither of which this
/// was measured on.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod adopt;
mod demo;
mod diff;
mod format;
mod identity_protect;
mod logging;
mod notify;
mod reset;
mod runview;
#[cfg(unix)]
mod sessions;
mod spark;
mod support;
mod term;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read as _, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use tokio::io::AsyncWriteExt;

use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::identity::Keypair;
use mesh_core::{ipc, membership, secrets, services};
use mesh_proto::{
    Capture, Grant, GrantId, GrantRevocation, IpcRequest, IpcResponse, JobId, NodeId,
    NodeRevocation, Role, Runtime, SecretRemoval, ServiceVersion, UserId, UserRecord,
    UserRevocation, read_frame,
};

mod cli;
use cli::*;

#[derive(Parser)]
#[command(
    name = "meshnode",
    version,
    about = "Meshware Node — every node is a server.",
    long_about = "Meshware Node: join Linux machines into an ad-hoc, governed compute mesh.\n\
                  One binary is both the daemon (`meshnode up`) and the CLI.",
    // The greeting is the eight verbs and nothing else (L1 P1). `help` is
    // ours rather than clap's because clap's takes no `--all`.
    disable_help_subcommand = true
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
#[command(next_help_heading = "Global options")]
struct GlobalArgs {
    /// Path to meshnode.toml (default: <home>/meshnode.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// The one directory this node keeps everything in — config, identity,
    /// state, jobs, content (default: ~/.meshware). See `config paths`.
    #[arg(
        long,
        visible_alias = "home",
        global = true,
        value_name = "DIR",
        env = "MESHWARE_HOME"
    )]
    data_dir: Option<PathBuf>,
    /// User identity key file to sign requests with
    /// (default: <home>/identity/user.key)
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        env = "MESHWARE_USER_IDENTITY"
    )]
    user_identity: Option<PathBuf>,
    /// Give up on a wedged daemon after this many seconds (0 = wait
    /// forever)
    ///
    /// A node that accepts the socket and then stalls used to hang every
    /// `meshnode` verb, and every script that shells out to one, silently
    /// and forever.
    //
    // `--ipc-timeout`, not `--timeout` (DOC-14): nine commands have a
    // `--timeout` of their own — a job's, a wait's — and each one hid this
    // option, so it could not be given to `run` at all. A distinct id is
    // what lets clap propagate it past a subcommand's own `timeout`.
    #[arg(
        id = "ipc_timeout",
        long = "ipc-timeout",
        global = true,
        value_name = "SECS",
        env = "MESHWARE_IPC_TIMEOUT"
    )]
    timeout: Option<u64>,
    /// Colour: `auto` on a terminal and never into a pipe, `always`, or
    /// `never`. `NO_COLOR` and `CLICOLOR_FORCE` are honoured under `auto`
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "WHEN",
        default_value = "auto"
    )]
    color: term::ColorChoice,
}

#[derive(Subcommand)]
enum Command {
    // The Focus strategy (2026-08) narrowed the product to the mesh and
    // the run. The surfaces it retired — workflows, inventory, change
    // control, quotas and the usage report — sat behind `MESHWARE_LEGACY=1`
    // for a while and have since been removed outright; see
    // docs/operating/legacy.md for what each became.
    //
    // Legibility L1 P1 then went further and made `hide` the default: the
    // greeting is the eight verbs of MASTERPLAN §1 and nothing else, so
    // the published mental model and `meshnode --help` say the same thing.
    // **`hide` is a discovery decision and nothing more** (D-L1.3). Every
    // command below still parses, still runs, still has a man page and
    // still completes — `full_command()` puts them all back for `help
    // --all`, `completion` and `manpage`, and the greeting's last line
    // says how many there are so an absence never reads as a removal.
    /// Start the node daemon (bootstraps identity + mesh on first run)
    #[command(hide = true)]
    Up {
        /// The address and port for mesh traffic (default: 0.0.0.0:7777)
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Option<String>,
        /// The new mesh's name, if this node starts one (default: <node name>-mesh)
        #[arg(long, value_name = "NAME")]
        mesh_name: Option<String>,
        /// This node's name in the mesh (default: the host name)
        #[arg(long, value_name = "NAME")]
        node_name: Option<String>,
        /// Node tags (repeatable), e.g. --tag gpu --tag dc=east
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Public endpoint this node is reachable on (repeatable), e.g. a
        /// cloud VM's elastic IP or a port-forward — `[wan] advertise`
        #[arg(long = "advertise", value_name = "HOST:PORT")]
        advertise: Vec<String>,
        /// Die when the process that started this one dies. Set by
        /// `meshnode demo` on the nodes it owns, and by nothing else
        #[arg(long, hide = true)]
        supervised: bool,
        /// Found a new mesh here even though this machine was reset out of
        /// its last one (`meshnode reset`). Without it, a reset machine waits
        /// to be adopted rather than founding a mesh of its own
        #[arg(long)]
        new_mesh: bool,
        /// Run the node in the background instead: as this account's
        /// `systemctl --user` service, installed on first use and started at
        /// every login. Returns once the node answers. Settings go in
        /// meshnode.toml, since the service is started without flags
        #[arg(
            long,
            short = 'd',
            conflicts_with_all = ["listen", "mesh_name", "node_name", "tags", "advertise", "supervised"]
        )]
        detach: bool,
    },
    /// Dissolve this mesh: every node leaves it and waits to be adopted
    /// into a new one (founder only; asks you to type a confirmation)
    #[command(
        hide = true,
        after_help = "Run on the founder's machine, signed with the founder's key. Every node\n\
                      stops its jobs and services, passes the reset on, moves the mesh's data\n\
                      into reset/ in its home (moved, not deleted) and stops, waiting to be\n\
                      adopted. Installs, services, configs and keys stay. Nodes that miss it\n\
                      are adopted into the new mesh with --force.\n\n  \
                      meshnode reset\n  \
                      meshnode --user-identity ~/.meshware/identity/user.key.set-aside-mwu1… reset\n\n\
                      Afterwards: `meshnode up --detach --new-mesh`, or Director, starts the new\n\
                      mesh here."
    )]
    Reset,
    /// Join an existing mesh: with an invite token, or --pair over the LAN
    /// (run before `up`)
    #[command(hide = true)]
    Join {
        /// Invite token (mwj1…); omit when using --pair
        token: Option<String>,
        /// AirDrop-style LAN pairing: announce this machine over mDNS and
        /// wait for a mesh member to approve with a 6-digit code
        #[arg(long)]
        pair: bool,
        /// How long to wait for a member when pairing (e.g. 10m)
        #[arg(long, value_name = "DURATION", default_value = "10m")]
        wait: String,
        /// The address this node will listen on when you run `up` — so the
        /// mesh records the right port immediately (defaults to config).
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Option<String>,
        /// Dial this address instead of the token's embedded ones
        /// (repeatable) — for VPN/Tailscale overlays where the minting
        /// node's advertised address is not the reachable one
        #[arg(long, value_name = "HOST:PORT")]
        via: Vec<String>,
        /// A tag for this node (repeatable), e.g. --tag gpu --tag dc=east
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// How to draw the confirmation code: a scannable QR (`blocks`),
        /// `ascii` for a console with no block glyphs, `off` for digits and
        /// words only. `auto` draws one on a terminal and none into a log
        #[arg(long, value_enum, default_value = "auto")]
        qr: QrStyle,
        /// Machine-readable pairing: emit NDJSON events on stdout and read
        /// the confirmation (`yes`/`no`) from stdin instead of prompting.
        /// What `meshnode adopt` drives over SSH; a person never needs it
        #[arg(long, requires = "pair")]
        json: bool,
    },
    /// grow the mesh — AirDrop-style, key-is-identity, seconds
    ///
    /// One verb for both ends of the handshake, because there is only one
    /// question — *are these two machines the same two machines?* — and
    /// which side you are on is a fact about this machine, not a decision
    /// you should have to make. In a mesh already, `pair` waits for a
    /// newcomer and admits it. Not in one, `pair` announces this machine
    /// and waits to be let in. Both sides read the same six digits.
    ///
    /// This is a front door onto `join --pair` and `invite approve`, which
    /// are unchanged and still work.
    #[command(after_help = "Examples:\n  \
                      meshnode pair                       # this machine picks the side it is on\n  \
                      meshnode pair --join                # wait to be let in, whatever this machine is\n  \
                      meshnode pair --approve             # let somebody in\n  \
                      meshnode pair --addr 10.0.0.9:7777  # skip mDNS on a multicast-hostile LAN\n\n\
                      Nothing is admitted until a person on each machine has read the same\n\
                      six digits — or the same two words — and said so. That comparison is\n\
                      the whole of the trust decision; there is no password anywhere in it.")]
    Pair {
        /// Wait to be let in: announce this machine and hold until a
        /// member approves it. The default when this machine has no mesh
        #[arg(long, conflicts_with = "approve")]
        join: bool,
        /// Let somebody in: browse for a machine that is waiting, compare
        /// codes, admit it. The default when this machine is in a mesh
        #[arg(long, conflicts_with = "join")]
        approve: bool,
        /// Which waiting machine, by name or id prefix, when several are
        /// announcing at once (approving side)
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// Reach the other machine at this address instead of discovering
        /// it — for a LAN that drops multicast, or across the internet
        #[arg(long, value_name = "HOST:PORT")]
        addr: Option<String>,
        /// How long to browse for machines that are waiting (approving side)
        #[arg(long, value_name = "DURATION", default_value = "3s")]
        browse: String,
        /// How long to hold, waiting for a member (joining side)
        #[arg(long, value_name = "DURATION", default_value = "10m")]
        wait: String,
        /// The address this node will listen on when you run `up`, so the
        /// mesh records the right port immediately (joining side)
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Option<String>,
        /// Tags for the machine being admitted (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// How to draw the confirmation code: a scannable QR (`blocks`),
        /// `ascii` for a console with no block glyphs, `off` for digits and
        /// words only. `auto` draws one on a terminal and none into a log
        #[arg(long, value_enum, default_value = "auto")]
        qr: QrStyle,
    },
    /// Watch it work: a real mesh on this machine, in about a minute
    ///
    /// Founds an ephemeral mesh of real `meshnode` daemons on 127.0.0.1,
    /// joins them to each other, fans a command out across all of them,
    /// then removes every trace when you stop it.
    ///
    /// These are the shipped daemon, the shipped scheduler and the shipped
    /// transport — not a simulation. What you see is what the product does,
    /// which is the only reason a demo is worth anything.
    ///
    /// Nothing touches ~/.meshware, nothing is announced on your network,
    /// and a demo node cannot join a real mesh or be joined by one.
    #[command(hide = true)]
    #[command(after_help = "Examples:\n  \
                      meshnode demo                # three machines on this one\n  \
                      meshnode demo --nodes 6      # six\n  \
                      meshnode demo --stop         # tear one down from another shell\n\n\
                      Nodes are Linux-only, so this command is too. On a Mac or a\n\
                      Windows machine, run Director and join a mesh that has a Linux node\n\
                      in it, or `meshnode adopt` a box you already have SSH to.")]
    Demo {
        /// How many machines to pretend to be (2–16)
        #[arg(long, default_value_t = 3, value_name = "N")]
        nodes: usize,
        /// Stop a demo that is still running, from any shell, and remove
        /// its scratch directory
        #[arg(long, conflicts_with = "nodes")]
        stop: bool,
    },
    /// This help. `--all` for every command, or name one to read its page
    ///
    /// `meshnode --help` greets you with the eight verbs the product is
    /// actually about. Nothing has been removed to make that list: every
    /// other command still runs, still completes and still has a man page,
    /// and `--all` prints all of them.
    #[command(hide = true)]
    Help {
        /// The command to explain — `run`, or `secrets set`
        #[arg(value_name = "COMMAND")]
        command: Vec<String>,
        /// Every command, including the ones the greeting does not list
        #[arg(long)]
        all: bool,
    },
    /// Compiler wrapper: spread a build across the mesh, cached
    #[command(hide = true)]
    #[command(
        after_help = "distcc + ccache, minus the setup folklore, plus the audit trail.\n\n\
                      Point your build system at it:\n  \
                      make -j64 CC='meshnode cc gcc' CXX='meshnode cc g++'\n  \
                      cmake -DCMAKE_C_COMPILER_LAUNCHER='meshnode cc' ...\n\n\
                      Each translation unit is preprocessed **here** — so the executor\n\
                      needs no headers, no checkout and no toolchain beyond a compiler —\n\
                      then submitted as a cacheable job. Because an inline script's action\n\
                      key is the digest of its own body, the same unit with the same flags\n\
                      is one key mesh-wide: the second machine to build it does not build\n\
                      it, and neither does the second person.\n\n\
                      **It never breaks the build.** A link, a `-E`, an assembler input,\n\
                      several sources, a stopped daemon, an empty mesh, a dropped\n\
                      connection — all of it falls back to compiling locally, verbatim. A\n\
                      real compile *error* does not fall back: the remote compiler's\n\
                      diagnostics are the answer, and building it twice to say the same\n\
                      thing would double the cost of every broken build.\n\n\
                      Run `meshnode cc --verbose ...` once to see where each object came\n\
                      from, and `meshnode cache stats` to see what the fleet is saving."
    )]
    Cc {
        /// Where compiles land: any | <name> | tag=<t>. Defaults to `any`,
        /// which is every node with room
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Say where each object came from, or why it stayed local
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Never distribute — still a cache hit if the mesh has one. What
        /// to try first when a build misbehaves
        #[arg(long)]
        local_only: bool,
        /// Per-compile timeout in seconds
        #[arg(long, value_name = "SECS", default_value = "600")]
        timeout: u64,
        /// The compiler and its arguments, exactly as the build system
        /// wrote them
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true,
            value_name = "COMPILER [ARG...]"
        )]
        argv: Vec<String>,
    },
    /// Adopt machines you already have SSH to: install, set up the
    /// service, pair, start — done (admin)
    #[command(
        after_help = "The pitch, made literal. Use the SSH access you already have **once** —\n\
                      install the binary, write the packaged systemd unit, pair with the\n\
                      standard confirmation code, start it — and from then on the mesh is the\n\
                      transport and SSH is never needed again.\n\n\
                      Root or passwordless sudo gets the server shape: /usr/local/bin, a\n\
                      `meshnode` service account, /var/lib/meshware, a system unit, the\n\
                      fleet-upgrade drop-in, and the mesh port opened in firewalld/ufw where\n\
                      one runs. Anything less gets the account's own home and a user unit,\n\
                      and says so. Director → Add machines does the same with a password.\n\n\
                      The confirmation code is not skipped. Pairing's security rests on the\n\
                      same six digits crossing a second channel an attacker in the middle of\n\
                      the pairing connection does not control; across a room that channel is a\n\
                      person's eyes, and here it is the SSH session — host-key verified, and\n\
                      already trusted enough to install a binary through. Both codes are\n\
                      printed. A mismatch abandons that host loudly and is never retried.\n\n\
                      Runs with `BatchMode=yes` and `StrictHostKeyChecking=ask`, so it never\n\
                      prompts and never accepts an unknown host key for you: SSH to a new\n\
                      machine once by hand first.\n\n\
                      Examples:\n  \
                      meshnode adopt ops@rack-{1..10}\n  \
                      meshnode adopt --tag gpu ops@gpu-{1,2}\n  \
                      meshnode adopt --no-sudo pi@shelf-{01..04}\n  \
                      meshnode adopt --binary ./meshnode-aarch64 pi@shelf-1\n  \
                      meshnode adopt --dry-run ops@rack-{1..10}"
    )]
    #[command(hide = true)]
    Adopt {
        /// `[user@]host` targets. Brace ranges work quoted or not:
        /// `ops@rack-{1..10}`, `{web,db}.local`, `pi@shelf-{01..04}`
        #[arg(value_name = "TARGET", required = true)]
        targets: Vec<String>,
        /// Hosts adopted at once
        #[arg(long, value_name = "N", default_value = "4")]
        parallel: usize,
        /// Push this binary instead of this machine's own — required when
        /// the target's architecture differs from this one
        #[arg(long, value_name = "PATH")]
        binary: Option<PathBuf>,
        /// Where the binary lands on each host (default: /usr/local/bin
        /// with root, ~/.local/bin without)
        #[arg(long, value_name = "PATH")]
        install_path: Option<String>,
        /// Insist on root or passwordless sudo — refuse a host that has
        /// neither rather than installing inside its home
        #[arg(long, conflicts_with = "no_sudo")]
        sudo: bool,
        /// Never use sudo, even where it would work without a password:
        /// install inside the account's home and run as that account
        #[arg(long)]
        no_sudo: bool,
        /// Skip the fleet-upgrade drop-in: this node will not replace its
        /// own binary for a mesh rollout, so its upgrades go over SSH
        #[arg(long)]
        no_fleet_upgrade: bool,
        /// Leave the firewall alone (the mesh port stays closed on a host
        /// running firewalld or ufw)
        #[arg(long)]
        no_firewall: bool,
        /// Tags for the adopted nodes (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Extra `ssh -o` option (repeatable), e.g. -o IdentityFile=~/.ssh/ops
        #[arg(short = 'o', long = "ssh-option", value_name = "OPTION")]
        ssh_options: Vec<String>,
        /// Dial the pairing port at this host instead of the SSH hostname —
        /// for a machine whose SSH name is not its data-plane address
        #[arg(long, value_name = "HOST")]
        via: Option<String>,
        /// Report what each host is and what would happen; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Adopt a host that already belongs to a mesh. This pairs a second
        /// identity on that machine; `meshnode upgrade` is what you want if
        /// you only meant to move its binary
        #[arg(long)]
        force: bool,
        #[arg(long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Show the running daemon's status
    #[command(hide = true)]
    Status {
        /// Add the mesh's spec sheet: machines, cores, memory, GPUs,
        /// poolable disk and what is running, summed across the mesh
        #[arg(long)]
        totals: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Show who you are to the mesh and what your grants allow
    #[command(hide = true)]
    Whoami {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What this mesh distributes, and how far it has spread
    #[command(hide = true)]
    Definitions {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Who is waiting to run here, behind whom, and why
    #[command(
        after_help = "The order shown is the order this node will actually run them:\n\
                      priority first, then whoever is furthest behind their fair share,\n\
                      then arrival.\n\n\
                      Priority stays the outer key on purpose. A share answers *whose\n\
                      turn*; a priority answers *this first*. A scheduler that let one\n\
                      quietly overrule the other would surprise people.\n\n\
                      SHARE is the dominant share \u{2014} lower means further behind, and\n\
                      further behind runs first. A mesh with no recent usage falls back to\n\
                      plain arrival order.\n\n\
                      This is one node's queue. Ask another node for its own."
    )]
    #[command(hide = true)]
    Queue {
        /// Group the queue by principal instead of listing every job
        #[arg(long)]
        by_principal: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// The replicated log: what it costs here, and what compacting it
    /// would recover
    #[command(
        after_help = "The op-log is append-only, and compaction is the one thing that takes\n\
                      anything back out of it. It is per-node and local: this node compacts\n\
                      the ops *it authored* and nobody else's, because an author is the only\n\
                      authority on its own sequence numbers.\n\n\
                      An op is dropped only when it is both superseded by a later op of the\n\
                      same author \u{2014} the same job's summary, the same host's record \u{2014} and\n\
                      older than `[state] compaction_horizon`. The op that won stays, so a\n\
                      node that never sees the dropped ones rebuilds exactly the same state.\n\n\
                      The audit log never compacts. It is tamper-evidence, and a compactable\n\
                      audit log is not one.\n\n\
                      Examples:\n  \
                      meshnode state show\n  \
                      meshnode state compact --dry-run\n  \
                      meshnode state compact --horizon 7d"
    )]
    #[command(hide = true)]
    State {
        #[command(subcommand)]
        cmd: Option<StateCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Send this mesh's history somewhere else — **optional**
    #[command(
        after_help = "Nothing needs this. The mesh keeps its own history, answers its own\n\
                      questions, and needs nothing else running \u{2014} that is the product. A sink\n\
                      is for the minority who need every run record in a warehouse they\n\
                      already own.\n\n\
                      A sink is admin-signed and replicated, so `meshnode audit` can answer\n\
                      who pointed the mesh's history at a host and when. Its credential is a\n\
                      {{secret:name}} reference resolved on the machine that connects, never a\n\
                      literal \u{2014} the record itself is replicated to every node.\n\n\
                      A failing sink spends spool, never scheduler. It never blocks a job, and\n\
                      when its spool fills it drops the oldest batches and counts them.\n\n\
                      Want the rows in a file? Use `plugin`: your program is handed the same\n\
                      NDJSON on stdin and decides where it goes, on the machine you chose.\n\n\
                      Examples:\n  \
                      meshnode sinks\n  \
                      meshnode sinks add warehouse --kind clickhouse --url https://ch:8443 \\\n    \
                        --database meshware --credential '{{secret:ch}}' --tables jobs\n  \
                      meshnode sinks add shipper --kind plugin --plugin /usr/local/bin/mw-ship\n  \
                      meshnode sinks rm warehouse\n  \
                      meshnode sinks resume    # after `meshnode restore`"
    )]
    #[command(hide = true)]
    Sinks {
        #[command(subcommand)]
        cmd: Option<SinksCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// How long this mesh keeps things, and what that settles the log at
    #[command(
        after_help = "meshware replicates its op-log to every node, so the storage cost of the\n\
                      mesh is (surviving history) \u{d7} (nodes). Compaction already drops what a later op\n\
                      supersedes; what it cannot drop is a *finished* job's last record, which\n\
                      supersedes nothing and is superseded by nothing. That needs a window.\n\n\
                      The window is replicated, admin-signed, and one for the whole mesh \u{2014} not\n\
                      a config file. Two nodes reading different horizons would answer the same\n\
                      question differently, and a history that depends on who you ask is not one.\n\n\
                      A node may keep *less* than the mesh window and never more, so the machine\n\
                      with the small disk stays the operator's to bound.\n\n\
                      Defaults are finite: runs 90d, facts 1y, job-output 30d, audit forever.\n\
                      `forever` is a thing you have to type. The audit window may be raised and\n\
                      never lowered below a year \u{2014} an audit log you can shorten to nothing is\n\
                      not an audit log.\n\n\
                      Examples:\n  \
                      meshnode retention show\n  \
                      meshnode retention set runs=30d\n  \
                      meshnode retention set runs=1y facts=forever --dry-run\n  \
                      meshnode retention reset"
    )]
    #[command(hide = true)]
    Retention {
        #[command(subcommand)]
        cmd: Option<RetentionCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Ask this mesh to recognise your key, and wait for an admin
    #[command(hide = true)]
    Enrol {
        /// The username you would like — a suggestion the approver sees
        #[arg(long = "as", value_name = "NAME")]
        as_: Option<String>,
        /// Print the code and exit instead of waiting for a decision
        #[arg(long)]
        no_wait: bool,
        /// How long to wait for a decision, in seconds
        #[arg(long, default_value_t = 3600, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Identity operations
    #[command(hide = true)]
    Identity {
        #[command(subcommand)]
        cmd: IdentityCmd,
    },
    /// Invite operations
    #[command(hide = true)]
    Invite {
        #[command(subcommand)]
        cmd: InviteCmd,
    },
    /// Mesh node inventory
    #[command(hide = true)]
    Nodes {
        #[command(subcommand)]
        cmd: Option<NodesCmd>,
        /// List every accelerator in the mesh instead of the nodes.
        /// Answered from replicated state, so it works from any node about
        /// every node
        #[arg(long)]
        devices: bool,
        /// With --devices: one node's accelerators, by name or id prefix
        #[arg(long, value_name = "NODE")]
        node: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// User management (admin)
    #[command(hide = true)]
    Users {
        #[command(subcommand)]
        cmd: Option<UsersCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Sign in through your organisation's identity provider
    #[command(hide = true)]
    #[command(
        after_help = "Proves to this mesh that the key on this machine belongs to you,\n\
                      according to the identity provider your company already runs.\n\n\
                      It does **not** create a login. meshware has no password, no session\n\
                      and no bearer token, and this adds none: you are still a key on your\n\
                      own machine. What login adds is evidence — the admin approving your\n\
                      enrolment sees a verified subject, email and group list instead of a\n\
                      52-character key you pasted into chat.\n\n\
                      Device-code flow, so nothing listens on this machine and it works\n\
                      over SSH on a headless box:\n\n  \
                      meshnode login entra\n  \
                      meshnode enrol --as mia\n\n\
                      If the provider is unreachable the mesh is unaffected — existing keys\n\
                      keep working and jobs keep running. The only thing that stops is new\n\
                      people enrolling *through SSO*, and they can still enrol the old way."
    )]
    Login {
        /// Which configured provider (`meshnode oidc providers`)
        provider: String,
        /// Give up after this long
        #[arg(long, value_name = "DURATION", default_value = "5m")]
        wait: String,
    },
    /// Identity providers this mesh believes (admin)
    #[command(hide = true)]
    Oidc {
        #[command(subcommand)]
        cmd: OidcCmd,
    },
    /// Machine principals: keys that belong to systems, not people (admin)
    #[command(
        name = "serviceaccounts",
        hide = true,
        alias = "service-accounts",
        alias = "serviceaccount",
        alias = "sa"
    )]
    #[command(after_help = "A pipeline should not authenticate as a person.\n\n\
                      `create` mints a fresh keypair, binds it to a name, marks it as a\n\
                      machine and grants it a scoped role — then prints the private key\n\
                      **once**, for you to paste into your CI secret store. meshware never\n\
                      stores it and cannot show it to you again.\n\n\
                      Examples:\n  \
                      meshnode serviceaccounts create ci-deploy --role developer \\\n    \
                          --on tag=staging --expires-in 90d\n  \
                      meshnode serviceaccounts ls\n  \
                      meshnode users revoke ci-deploy --yes      # same verb as a person\n\n\
                      Two clocks, and they are different. A *grant* expiring leaves a live\n\
                      key with no roles, which an admin can re-grant. An *account* expiring\n\
                      is terminal for that credential: re-granting it does nothing, because\n\
                      the principal itself is out of time. That is the one you want on a CI\n\
                      token, which is why --expires-in is required.\n\n\
                      A service account may not hold the admin role. An admin key can mint\n\
                      users and grants, so a leaked pipeline credential holding it is\n\
                      permanent mesh compromise.")]
    ServiceAccounts {
        #[command(subcommand)]
        cmd: Option<ServiceAccountsCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Scoped role grants (admin)
    #[command(hide = true)]
    Grants {
        #[command(subcommand)]
        cmd: Option<GrantsCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// feed it safely — envelope-encrypted, late-binding
    ///
    /// A value is sealed on the machine you type it on and is opened only on
    /// the node that runs the job that needs it, at the moment it needs it.
    /// The mesh replicates the sealed form; nothing in between ever holds the
    /// plaintext.
    Secrets {
        #[command(subcommand)]
        cmd: SecretsCmd,
    },
    /// Query the replicated audit log
    #[command(hide = true)]
    Audit {
        /// Filter by username or mwu1… id
        #[arg(long)]
        user: Option<String>,
        /// Filter by action prefix (e.g. job. or secret.resolve)
        #[arg(long)]
        action: Option<String>,
        /// Only events newer than this (e.g. 24h, 30m, 7d)
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// How many entries to show, newest first
        #[arg(long, default_value = "100")]
        limit: u32,
        /// table | json | jsonl (jsonl suits SIEM export)
        #[arg(long, default_value = "table")]
        output: String,
    },
    /// Reach a peer: handshake + round-trip over encrypted QUIC.
    ///
    /// A mesh node (by name or id) is pinged through the ordinary dialer,
    /// which races every address it announced and falls back to a relay.
    /// Anything else — `host:port`, an IP — is probed as a raw address.
    #[command(hide = true)]
    Ping {
        /// <node> | <name> | <id-prefix> | host:port
        #[arg(value_name = "TARGET")]
        addr: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// everything, everywhere — streaming, cancel, fan-out, results as a table
    ///
    /// One command, on one node or on all of them, with its output arriving
    /// as it is produced rather than when it is finished. `--fanout` turns
    /// one invocation into one job per matching machine and collects the
    /// answers into a table.
    Run {
        /// The command to run
        command: String,
        /// Target selector: any | <name> | name=<n> | tag=<t> | id=<mw1..>
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Environment KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Timeout in seconds
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Fan out to matching nodes as one logical job: `all` or a count
        #[arg(long, value_name = "all|N")]
        fanout: Option<String>,
        /// Queue priority on the executor: low | normal | high
        #[arg(long, default_value = "normal")]
        priority: String,
        /// Fan-out: at most this many targets in flight at once
        #[arg(long, value_name = "N")]
        max_parallel: Option<u32>,
        /// Fan-out: stop dispatching after N failures (or N% of targets)
        #[arg(long, value_name = "N|N%")]
        halt_on_fail: Option<String>,
        /// Whole cores this task needs
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// Memory this task needs, e.g. 512M, 48G
        #[arg(long, value_name = "SIZE")]
        mem: Option<String>,
        /// Scratch disk this task needs, e.g. 20G
        #[arg(long, value_name = "SIZE")]
        disk: Option<String>,
        /// This work may be killed to give another principal back their
        /// fair share. Opt-in, and never a default
        #[arg(long)]
        preemptible: bool,
        /// Accelerators this task needs, exclusively
        #[arg(long, value_name = "N")]
        gpus: Option<u32>,
        /// Minimum memory on *each* device, in MB — a box with four 12 GB
        /// cards does not satisfy `--vram-mb 40000`
        #[arg(long = "vram-mb", value_name = "MB")]
        vram_mb: Option<u64>,
        /// Device selector: `kind=gpu,model=~4090,vendor=nvidia`. `~x`
        /// means "contains x"
        #[arg(long, value_name = "K=V,...")]
        device: Option<String>,
        /// Host ports this job needs: `http:9095`, or `http:8080!` for a
        /// fixed one. Repeatable. The allocation comes back as
        /// MESHWARE_PORT_<LABEL>, MESHWARE_ADDR_<LABEL> and
        /// MESHWARE_IP_<LABEL> in the job's environment
        #[arg(long, value_name = "LABEL:PORT")]
        port: Vec<String>,
        /// Register this job in the service registry under NAME, so
        /// `NAME.mesh` resolves to it while it runs. Needs a --port — a
        /// name with no address resolves to nothing
        #[arg(long, value_name = "NAME")]
        register: Option<String>,
        /// Claim a volume this node lends: `data`, `data:ro`, or
        /// `data:/mnt/data:rw` to say where a container mounts it.
        /// Repeatable
        #[arg(long = "volume", value_name = "NAME[:PATH][:MODE]")]
        volumes: Vec<String>,
        /// Tolerate co-tenants on the devices this job is given. Opt-in,
        /// because silent sharing produces OOM failures that look like
        /// application bugs in somebody else's job
        #[arg(long = "gpu-share")]
        gpu_share: bool,
        /// Run as an all-or-nothing gang of N nodes: every member starts at
        /// the same moment, or none of them does
        #[arg(long, value_name = "N")]
        gang: Option<u32>,
        /// Gang layout: `pack` onto the fewest nodes (default, shortest
        /// inter-task path) or `spread` one per node for fault tolerance
        #[arg(long = "gang-placement", value_name = "pack|spread")]
        gang_placement: Option<String>,
        /// Re-run the whole gang if a member dies, instead of failing it
        #[arg(long = "gang-restart")]
        gang_restart: bool,
        /// Resolve and print what this would do — targets, env keys, secret
        /// names and blast radius — without dispatching anything
        #[arg(long)]
        dry_run: bool,
        /// Capture each target's answer and print one JSON array of rows
        /// instead of interleaved logs
        #[arg(long)]
        json: bool,
        /// Also capture these `::meshware-output::` keys, comma-separated.
        /// The four every job publishes — stdout, stdout_json, stderr,
        /// exit_code — come for free
        #[arg(long, value_name = "K,V")]
        outputs: Option<String>,
        /// Assert something about every row and exit non-zero if any fails,
        /// e.g. 'contains(stdout, "permitrootlogin no")'
        #[arg(long, value_name = "EXPR")]
        expect: Option<String>,
        /// Serve this from the action cache if the same work has already
        /// been done anywhere in the mesh, and publish the result if it has
        /// not. A claim you are making about your own code: it must be a
        /// pure function of its inputs
        #[arg(long)]
        cacheable: bool,
        /// Files the action produces, comma-separated globs relative to the
        /// working directory. Part of the cache key and what a hit
        /// materialises
        #[arg(long = "cache-outputs", value_name = "GLOB,...")]
        cache_outputs: Option<String>,
        /// Print the action key and every field that produced it, and
        /// dispatch nothing. Two runs whose keys match are the same work
        #[arg(long = "explain-key")]
        explain_key: bool,
        /// Run this on N nodes at once and keep the first answer, cancelling
        /// the rest. Latency, not throughput: the job returns as fast as the
        /// fastest machine in the mesh rather than the one it landed on.
        /// **Your command runs N times** — only race work that is a pure
        /// function of its inputs
        #[arg(long, value_name = "N")]
        race: Option<u32>,
        /// Refuse this run while another job under the same name is still
        /// going, anywhere in the mesh — so a schedule that fires while its
        /// last run is still busy does not start a second copy. The name is
        /// free again the moment the job ends, however it ends
        #[arg(long, value_name = "NAME")]
        singleton: Option<String>,
        /// What to do when the mesh cannot take this run: `local` runs the
        /// command right here, in this shell, the way `meshnode cc` falls
        /// back to compiling here — for a laptop that is sometimes on the
        /// mesh and sometimes on a train. Only when no daemon answers or no
        /// live node can run it; a refusal by a grant is never worked around
        #[arg(long, value_name = "local")]
        fallback: Option<String>,
        /// Tell me when it is done: a desktop notification, an ntfy or
        /// webhook URL, or a command — whatever `[notify]` in meshnode.toml
        /// names. Nothing to do with sinks; this is one message, to you
        #[arg(long)]
        notify: bool,
        /// Fan-out: print every machine's lines the moment they arrive,
        /// interleaved, instead of holding each machine's output until it
        /// finishes. For output you want to watch live — `tail -f`, a
        /// long build
        #[arg(long)]
        stream: bool,
    },
    // -- the fabric (M17 §3.1) --------------------------------------------
    //
    // Four verbs that turn the transport into reach. Everything hard was
    // already built — key-pinned QUIC, hole punching, relays, reverse
    // streams — and a laptop behind CGNAT could already join a cloud mesh
    // with one token and not be shelled into. That was an omission, not a
    // design.
    /// a hand on any node, through the mesh, audited — no SSH
    ///
    /// A real interactive terminal on a mesh member, carried over the mesh's
    /// own encrypted transport. No sshd, no key distribution, no second
    /// account system — and every session is in the audit log with the
    /// reason you gave for opening it.
    #[command(after_help = "Examples:\n  \
                      meshnode shell hermes\n  \
                      meshnode shell hermes -- htop\n\n\
                      Needs the same grant as `meshnode run` on that node: a user who cannot\n\
                      run a command there cannot get a prompt there, and vice versa.")]
    Shell {
        /// Member to reach, by name or node id
        node: String,
        /// Why — a ticket, an incident, a sentence. Recorded in the audit
        /// log and covered by your signature
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Run this under a terminal instead of the login shell
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Live sessions on this node
    #[command(hide = true)]
    Sessions {
        #[command(subcommand)]
        cmd: Option<SessionCmd>,
    },
    /// Run one command per input item across the whole mesh, GNU-parallel
    /// style
    #[command(
        after_help = "Items come from -a/--arg-file, from a `:::` list, or from stdin.\n\
                      `{}` in the command is replaced by each item, as in GNU parallel.\n\n\
                      Every option goes BEFORE the command: everything from the command on\n\
                      is passed through untouched, as GNU parallel does it.\n\n\
                      Examples:\n  \
                      meshnode parallel -a hosts.txt -- ./check.sh {}\n  \
                      meshnode parallel 'gzip -t {}' ::: logs/*.gz\n  \
                      seq 1 1000 | meshnode parallel 'process {}'"
    )]
    #[command(hide = true)]
    Parallel {
        /// Read items from this file, one per line (`-` = stdin)
        #[arg(short = 'a', long = "arg-file", value_name = "FILE")]
        arg_file: Option<String>,
        /// Target selector: any | <name> | name=<n> | tag=<t> | id=<mw1..>
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Environment KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Per-item timeout in seconds
        #[arg(long, visible_alias = "item-timeout", value_name = "SECS")]
        timeout: Option<u64>,
        /// Stop handing out work once this many items (or this share of
        /// them) have failed — a circuit breaker for a script with a bug
        /// in it, pointed at eight hundred hosts. What is in flight
        /// finishes and reports; the rest is untouched, and `--resume`
        /// picks it up
        #[arg(long = "halt-on-fail", visible_alias = "max-fail", value_name = "N|N%")]
        halt_on_fail: Option<String>,
        /// Re-run only the items of an earlier run that did not succeed:
        /// failed, timed out, or never dispatched. The command, selector,
        /// requirements and environment come from that run's record, so
        /// give no command and no items
        #[arg(long, value_name = "JOB", conflicts_with_all = ["arg_file", "service"])]
        resume: Option<String>,
        /// Whole cores each item needs
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// Memory each item needs, e.g. 512M, 48G
        #[arg(long, value_name = "SIZE")]
        mem: Option<String>,
        /// Scratch disk each item needs, e.g. 20G
        #[arg(long, value_name = "SIZE")]
        disk: Option<String>,
        /// Run a Library script once per item instead of a shell command
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
        /// key=value inputs for --service (validated against its contract)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Which of the script's inputs receives each item
        #[arg(long, value_name = "KEY")]
        item_input: Option<String>,
        /// Items per chunk. Default: adaptive, a few seconds of work each
        #[arg(long, value_name = "N")]
        chunk: Option<u32>,
        /// Print a per-node throughput summary while the job runs
        #[arg(long)]
        progress: bool,
        /// Resolve the workers and print what would run, without
        /// dispatching anything
        #[arg(long)]
        dry_run: bool,
        /// Serve each item from the action cache if the same item has
        /// already been computed anywhere in the mesh, and publish it if it
        /// has not. A claim you are making about your own command: each
        /// item must be a pure function of its inputs
        #[arg(long)]
        cacheable: bool,
        /// The command, then optionally `:::` and a literal item list
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND [::: ITEM...]"
        )]
        rest: Vec<String>,
    },
    /// a million items across the mesh — data stays where it was made
    ///
    /// One command per input item, spread over every machine with room, and
    /// each item's output kept as an object *on the node that computed it*.
    /// Only a reference travels back, so intermediate data never funnels
    /// through this machine, and a `--reduce` runs where most of that data
    /// already is.
    #[command(
        after_help = "`map` is `parallel` with one difference that changes everything:\n\
                      each item's output becomes an ephemeral object on the node that\n\
                      computed it, and only a reference travels back. A --reduce step then\n\
                      runs where most of that output already is.\n\n\
                      An item may write its output to $MESHWARE_OUT; if it writes nothing\n\
                      there, its stdout is kept instead — so an ordinary command needs no\n\
                      modification. A reduce reads its inputs from $MESHWARE_INPUTS, one\n\
                      file per map output.\n\n\
                      Every option goes BEFORE the command: everything from the command on\n\
                      is passed through untouched, as GNU parallel does it.\n\n\
                      Examples:\n  \
                      meshnode map 'gzip -dc {} | grep -c ERROR' ::: logs/*.gz\n  \
                      meshnode map -a files.txt --reduce 'sort -u \"$MESHWARE_INPUTS\"/*' 'grep WARN {}'\n  \
                      meshnode map -a frames.txt --out ./renders/ './render.sh {}'"
    )]
    Map {
        /// Read items from this file, one per line (`-` = stdin)
        #[arg(short = 'a', long = "arg-file", value_name = "FILE")]
        arg_file: Option<String>,
        /// Target selector: any | <name> | name=<n> | tag=<t> | id=<mw1..>
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Environment KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Per-item timeout in seconds
        #[arg(long, visible_alias = "item-timeout", value_name = "SECS")]
        timeout: Option<u64>,
        /// Stop handing out work once this many items (or this share of
        /// them) have failed — a circuit breaker for a script with a bug
        /// in it, pointed at eight hundred hosts. What is in flight
        /// finishes and reports; the rest is untouched, and `--resume`
        /// picks it up
        #[arg(long = "halt-on-fail", visible_alias = "max-fail", value_name = "N|N%")]
        halt_on_fail: Option<String>,
        /// Re-run only the items of an earlier run that did not succeed:
        /// failed, timed out, or never dispatched. The command, selector,
        /// requirements and environment come from that run's record, so
        /// give no command and no items
        #[arg(long, value_name = "JOB", conflicts_with_all = ["arg_file", "service"])]
        resume: Option<String>,
        /// Whole cores each item needs
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// Memory each item needs, e.g. 512M, 48G
        #[arg(long, value_name = "SIZE")]
        mem: Option<String>,
        /// Scratch disk each item needs, e.g. 20G
        #[arg(long, value_name = "SIZE")]
        disk: Option<String>,
        /// Run a Library script once per item instead of a shell command
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
        /// key=value inputs for --service (validated against its contract)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Which of the script's inputs receives each item
        #[arg(long, value_name = "KEY")]
        item_input: Option<String>,
        /// Items per chunk. Default: adaptive, a few seconds of work each
        #[arg(long, value_name = "N")]
        chunk: Option<u32>,
        /// Merge command run once over every map output, reading them from
        /// $MESHWARE_INPUTS
        #[arg(long, value_name = "COMMAND")]
        reduce: Option<String>,
        /// Where the reduce runs. Default: the node already holding the
        /// most output, so the least data moves
        #[arg(long, value_name = "SELECTOR")]
        reduce_on: Option<String>,
        /// Fetch every output into this directory. Without it the outputs
        /// stay where they were produced and only their ids are printed —
        /// which is the point of `map`
        #[arg(long, value_name = "DIR")]
        out: Option<String>,
        /// Print a per-node throughput summary while the job runs
        #[arg(long)]
        progress: bool,
        /// Serve each item from the action cache if the same item has
        /// already been computed anywhere in the mesh, and publish it if it
        /// has not. A claim you are making about your own command: each
        /// item must be a pure function of its inputs
        #[arg(long)]
        cacheable: bool,
        /// The command, then optionally `:::` and a literal item list
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND [::: ITEM...]"
        )]
        rest: Vec<String>,
    },
    /// Ephemeral object operations
    #[command(hide = true)]
    Objects {
        #[command(subcommand)]
        cmd: Option<ObjectsCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Job operations
    #[command(hide = true)]
    Jobs {
        #[command(subcommand)]
        cmd: Option<JobsCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Put a script on the mesh so it can be run by name
    #[command(hide = true)]
    #[command(after_help = "\
Replaces push + service create + a hand-written JSON Schema with one verb.

  meshnode add ./transcode-media.sh
  meshnode add ./tools --entry build.sh --name nightly-build

Arguments come from the script's own `#meshware:` header, or are inferred
from the flags it reads. `meshnode script inspect FILE` shows what will be
registered without registering anything.")]
    Add {
        /// The script to add, or a directory containing it
        path: String,
        /// Service name. Default: the header's `name`, else the filename
        #[arg(long)]
        name: Option<String>,
        /// Entry point within a directory
        #[arg(long)]
        entry: Option<String>,
        /// Runtime. Default: from the file extension
        #[arg(long)]
        runtime: Option<String>,
    },
    /// What a script says about itself, and what it would register as
    #[command(hide = true)]
    Script {
        #[command(subcommand)]
        cmd: ScriptCmd,
    },
    /// Push a local directory into the mesh content store as a bundle
    #[command(hide = true)]
    Push {
        /// Directory to ingest
        path: String,
    },
    /// Bundle operations
    #[command(hide = true)]
    Bundles {
        #[command(subcommand)]
        cmd: Option<BundlesCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What this node's content store holds, and reclaiming it
    #[command(
        hide = true,
        after_help = "The content store is the only thing a node keeps that grows without\n\
                      a bound of its own: every release, every source sync, every bundle\n\
                      anyone ever deployed here. `content` is where you see that and\n\
                      take it back.\n\n\
                      A reclaim never touches a bundle something still needs — a live\n\
                      service version, a deployment's pinned version, a git source head,\n\
                      or this node's own release. Those are protected at every level,\n\
                      including `--aggressive`.\n\n\
                      Examples:\n  \
                      meshnode content                    # what is held, what is reclaimable\n  \
                      meshnode content prune --dry-run    # what a reclaim would take\n  \
                      meshnode content prune              # take it\n  \
                      meshnode content prune --aggressive # everything nothing needs, now\n\n\
                      How hard the hourly sweep works is `[retention] content` in\n\
                      meshnode.toml: keep | relaxed | balanced | tidy | lean | ephemeral.\n\
                      The mesh-wide ceiling is `meshnode retention set content=30d`."
    )]
    Content {
        #[command(subcommand)]
        cmd: Option<ContentCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Free space on every node, worst first
    #[command(hide = true)]
    Disk {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Service operations
    #[command(hide = true)]
    Service {
        #[command(subcommand)]
        cmd: Option<ServiceCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// The action cache: what the mesh has already done, and did not do
    /// again
    #[command(hide = true)]
    Cache {
        #[command(subcommand)]
        cmd: Option<CacheCmd>,
    },
    /// What-if: where this would land, when it would finish, and whose work
    /// it would delay — submitting nothing
    #[command(after_help = "Examples:\n  \
                      meshnode plan --on tag=gpu --gpus 2\n  \
                      meshnode plan --service train --on any\n\n\
                      `--dry-run` on `run` answers the governance question — targets, blast\n\
                      radius, verdict. This answers the capacity one.")]
    #[command(hide = true)]
    Plan {
        /// Target selector: any | <name> | name=<n> | tag=<t> | id=<mw1..>
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Plan a service run rather than an ad-hoc command
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
        /// Whole cores this task needs
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// Memory this task needs, e.g. 512M, 48G
        #[arg(long, value_name = "SIZE")]
        mem: Option<String>,
        /// Scratch disk this task needs, e.g. 20G
        #[arg(long, value_name = "SIZE")]
        disk: Option<String>,
        /// Accelerators this task needs
        #[arg(long, value_name = "N")]
        gpus: Option<u32>,
        /// Minimum memory on each device, in MB
        #[arg(long = "vram-mb", value_name = "MB")]
        vram_mb: Option<u64>,
        /// Device selector: `kind=gpu,model=~4090,vendor=nvidia`
        #[arg(long, value_name = "K=V,...")]
        device: Option<String>,
        /// Plan a fan-out to this many targets: `all` or a count
        #[arg(long, value_name = "all|N")]
        fanout: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Gangs: all-or-nothing reservations in flight, and who is senior to
    /// whom
    #[command(visible_alias = "gang")]
    #[command(hide = true)]
    Gangs {
        #[command(subcommand)]
        cmd: Option<GangCmd>,
    },
    /// Deployments: services that are supposed to stay up
    #[command(visible_alias = "deployments")]
    #[command(hide = true)]
    Deployment {
        #[command(subcommand)]
        cmd: Option<DeploymentCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What is registered and where it is — the service registry
    // Not a *visible* alias: listed, `services` sat beside `service`, a
    // different command, and the two were easy to confuse (DOC-14).
    #[command(alias = "services")]
    #[command(hide = true)]
    Registry {
        /// One service's instances, rather than the summary
        #[arg(value_name = "SERVICE")]
        service: Option<String>,
        /// What this node's DNS responder is doing, and on which port
        #[arg(long, conflicts_with = "service")]
        dns: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Make `<name>.mesh` resolve and answer on this machine
    #[command(hide = true)]
    Dns {
        #[command(subcommand)]
        cmd: DnsCmd,
    },
    /// Stop the mesh taking work, mesh-wide, without touching a node
    #[command(hide = true)]
    Shed {
        #[command(subcommand)]
        cmd: ShedCmd,
    },
    /// Volumes this node lends, and what is holding them
    #[command(hide = true)]
    Volumes {
        /// Provision or remove one. Absent = list.
        #[command(subcommand)]
        cmd: Option<VolumeCmd>,
        /// Which node. Absent = the one this CLI talks to. A claim is the
        /// node's own lease table, so there is no mesh-wide answer.
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What this node can run — containers, python, ansible
    #[command(hide = true)]
    Runtimes {
        /// Which node's own probes. Absent = the one this CLI talks to. A
        /// probe is node-local; every node's *announced* runtimes appear
        /// either way.
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Ports this node lends, and what is holding them
    #[command(hide = true)]
    Ports {
        /// Which node. Absent = the one this CLI talks to. A ledger is
        /// node-local until the service registry lands.
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Schedules: a service that runs itself, on a clock
    #[command(visible_alias = "schedules")]
    #[command(hide = true)]
    Schedule {
        #[command(subcommand)]
        cmd: Option<ScheduleCmd>,
        /// For the default `ls`
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// feel it — live saturation, no refresh
    ///
    /// Per-node load, running jobs and queue depth, updating in place.
    Top {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
        /// Keep it open: redraw every --interval seconds, with a sparkline
        /// of the last few minutes of CPU beside each node — so a number
        /// reads as ramping, saturated or idling down
        #[arg(long, short = 'f', visible_alias = "watch")]
        follow: bool,
        /// Seconds between redraws with --follow
        #[arg(long, default_value = "2", value_name = "SECS")]
        interval: u64,
        /// The cards instead of the nodes: one row per accelerator, with
        /// utilisation, memory and temperature as bars. With --follow, a
        /// sparkline of each card's utilisation
        #[arg(long)]
        devices: bool,
    },
    /// prove the collective — `--mesh` scores the whole thing
    ///
    /// Eight deterministic kernels, a score and a report. On its own it
    /// measures this machine and runs standalone — no daemon, no mesh, no
    /// config, because the moment you most want a machine's number is before
    /// you have added it to anything. With `--mesh` it measures the mesh: the
    /// parallel-efficiency figure is what the collective is actually worth.
    #[command(after_help = "\
Examples:\n  \
meshnode bench                                  # this machine, standard profile\n  \
meshnode bench --mesh                           # the whole mesh: verify, baseline, swarm, score\n  \
meshnode bench --mesh --on tag=epyc             # one rack against the clock\n  \
meshnode bench --profile quick                  # ~45s\n  \
meshnode bench --load 100 --memory 90           # push harder\n  \
meshnode bench --json > node.json               # the document a workflow reads\n  \
meshnode bench verify                           # prove the kernels, measure nothing\n  \
meshnode bench report mesh.json -o report.html  # render a finished run")]
    /// How much machine this mesh adds up to
    #[command(hide = true)]
    Totals {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Git sources: replicated bundles with Git as the upstream
    #[command(hide = true)]
    Sources {
        #[command(subcommand)]
        cmd: Option<SourcesCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What the mesh knows about a machine, because the machine said so
    #[command(after_help = "\
Facts are what a node observes about itself, not what somebody typed.\n\
The built-in `system` collector runs on every node; `collectors add`\n\
publishes your own.\n\n\
Examples:\n  \
meshnode facts                              # this node\n  \
meshnode facts hermes --collector system\n  \
meshnode facts query 'os_id=debian' --count\n  \
meshnode facts diff hermes --since 7d\n  \
meshnode run 'apt-get -y upgrade' --on 'fact.os_id=debian'")]
    // `args_conflicts_with_subcommands` is what makes `meshnode facts
    // hermes` and `meshnode facts query …` both work: the bare form takes
    // a subject, and a word that names a subcommand is one.
    #[command(args_conflicts_with_subcommands = true)]
    #[command(hide = true)]
    Facts {
        /// A node name, node id prefix, or host name (default: this node)
        subject: Option<String>,
        /// One collector only
        #[arg(long)]
        collector: Option<String>,
        #[command(subcommand)]
        cmd: Option<FactsCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Fact collectors: what each node runs to describe itself
    #[command(hide = true)]
    Collectors {
        #[command(subcommand)]
        cmd: Option<CollectorsCmd>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// The rows one fan-out answered with
    ///
    /// **The spelling is now `meshnode history results`**, with every other
    /// question about the past. This one keeps working and is not
    /// advertised; it goes in a later release.
    #[command(hide = true)]
    #[command(args_conflicts_with_subcommands = true)]
    Results {
        /// The fan-out group id, as `meshnode run --fanout` printed it
        group: Option<String>,
        /// Only targets whose job did not succeed
        #[arg(long)]
        failed: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
        #[command(subcommand)]
        cmd: Option<ResultsCmd>,
    },
    /// Search what this node has already run
    #[command(hide = true)]
    History {
        #[command(subcommand)]
        cmd: Option<HistoryCmd>,
    },
    /// Configuration operations
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// leave politely — finish accepted work, refuse new
    ///
    /// Stop accepting work, finish (or move) what was already accepted, then
    /// exit clean. The polite way to take a machine back: nothing in flight
    /// is lost and nothing new is started.
    #[command(after_help = "Examples:\n  \
                      meshnode drain                      # finish accepted work, refuse new, exit\n  \
                      meshnode drain --move               # …moving queued jobs elsewhere first\n  \
                      meshnode drain --until 06:00 --reason 'kernel update'\n  \
                      meshnode drain --for 2h             # a maintenance window: back on its own\n  \
                      meshnode drain --lift               # end a timed drain early\n  \
                      meshnode drain --deadline 30m       # stop whatever is left after 30m\n\n\
                      Deployment instances move a replacement first, then the original, under\n\
                      each deployment's `migrate` rules, and are stopped at the deadline — one\n\
                      hour unless --deadline says. To empty another node without stopping it,\n\
                      use `meshnode nodes drain <node>`.")]
    Drain {
        /// Re-dispatch still-queued jobs to other nodes before waiting
        #[arg(long = "move")]
        move_pending: bool,
        /// Give up waiting after this long (e.g. 30m, 0s): deployment
        /// instances have until then to move, and whatever still runs here is
        /// stopped when it passes. Without it, instances get an hour and
        /// other work is waited for however long it takes
        #[arg(long, value_name = "DURATION", conflicts_with_all = ["until", "for_", "lift"])]
        deadline: Option<String>,
        /// A drain with an end: refuse new work until this local time
        /// (HH:MM, later today or tomorrow), keep what is running, then
        /// rejoin on your own. The daemon stays up
        #[arg(long, value_name = "HH:MM", conflicts_with_all = ["for_", "lift"])]
        until: Option<String>,
        /// Like --until, for a duration from now (e.g. 2h, 45m)
        #[arg(long = "for", value_name = "DURATION", conflicts_with = "lift")]
        for_: Option<String>,
        /// Why — shown in `status` and the audit log
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// End a timed drain now
        #[arg(long)]
        lift: bool,
    },
    /// Collect one file that answers the first hour of a support question
    ///
    /// Version, host, `doctor`, this node's status, every peer and what it
    /// is running, a tail of the audit log and a `/metrics` snapshot —
    /// written to a single JSON file you can attach to a message.
    ///
    /// Every section degrades on its own, because a bundle is wanted
    /// exactly when something is broken: a node that will not start still
    /// produces one carrying the config, the host and every `doctor` check
    /// that needs no socket, plus a note saying what was missing and why.
    ///
    /// Credentials are redacted by key name and webhook paths are stripped.
    /// Node names, addresses, user ids and the audit tail are not — this is
    /// a redacted file, not a harmless one. Read it before sending it.
    ///
    /// Hidden from the greeting for the same reason `doctor` and `backup`
    /// are: the greeting lists the eight verbs of a normal day, and this
    /// is a verb for an abnormal one. `help --all`, completions and the
    /// man pages all carry it.
    #[command(hide = true)]
    Support {
        #[command(subcommand)]
        cmd: SupportCmd,
    },
    /// Health checks: connectivity, clock skew, disk, permissions, sandbox
    /// capability probes
    #[command(hide = true)]
    Doctor {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
        /// Apply the safe fixes the checks already know — create the data
        /// dir, resume sinks a restore held, restart a daemon systemd
        /// manages so it runs this build — and say plainly which findings
        /// have no automatic fix and what to do instead
        #[arg(long)]
        fix: bool,
    },
    /// Write a snapshot of this node (identity, genesis, full op-log) to a
    /// single restorable file
    #[command(hide = true)]
    Backup {
        /// Output file (default: meshware-backup-<node>.mwb in the cwd)
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
    /// Restore a node from a backup file into an empty data dir (run
    /// before `up`; the daemon must not be running)
    #[command(hide = true)]
    Restore {
        /// Backup file written by `meshnode backup`
        file: String,
        /// Required: restoring writes identity keys and mesh state
        #[arg(long)]
        yes: bool,
    },
    /// Generate shell completions
    #[command(hide = true)]
    Completion {
        /// The shell to write completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Write the man pages: `meshnode.1` and one per subcommand
    #[command(hide = true)]
    Manpage {
        /// Directory to write them into. Without it, the main page goes
        /// to stdout (`meshnode manpage | man -l -`)
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Upgrade this binary from the signed release channel and restart the
    /// daemon through a drain
    #[command(hide = true)]
    Upgrade {
        /// Upgrade the meshnode binary itself (required unless --on)
        #[arg(long = "self")]
        self_: bool,
        /// Roll the release across these nodes instead, one at a time, with
        /// a health gate between each. `any` is refused: name a ring
        /// (tag=canary) and widen it. `,` is all-of, `|` is any-of —
        /// `tag=cap.ver=0.5.0` is every node on that build, and
        /// `name=anvil|name=edge-7` is a hand-picked pair.
        #[arg(long, value_name = "SELECTOR", conflicts_with = "self_")]
        on: Option<String>,
        /// How many nodes may upgrade at once. 1 (the default) is strictly
        /// serial; raise it for a large fleet — the health gate still holds
        /// at every wave boundary
        #[arg(long, default_value_t = 1, value_name = "N")]
        batch: u32,
        /// Show what each node is running and what is outdated, then stop
        #[arg(long, conflicts_with_all = ["self_", "on"])]
        status: bool,
        /// Publish this binary into the mesh as the release for --version, so
        /// nodes fetch it from each other instead of from a server
        #[arg(long, value_name = "FILE", conflicts_with_all = ["self_", "on"])]
        publish: Option<String>,
        /// The .minisig for --publish (default: <FILE>.minisig)
        #[arg(long = "publish-sig", value_name = "FILE")]
        publish_sig: Option<String>,
        /// Publish a hardware co-signature for --version's release
        ///
        /// The document `ssh-keygen -Y sign -n meshware-release <binary>`
        /// produced. Verified against the SSH-form release keys this mesh
        /// trusts before it is recorded, so publishing one made by a key
        /// nobody trusts records nothing.
        #[arg(long = "cosign", value_name = "FILE", conflicts_with_all = ["self_", "on"])]
        cosign_publish: Option<String>,
        /// List the releases this mesh holds, then stop
        #[arg(long, conflicts_with_all = ["self_", "on"])]
        releases: bool,
        /// List the rollout plans this mesh holds and stop
        #[arg(long, conflicts_with_all = ["self_", "on"])]
        plans: bool,
        /// Trust a minisign public key mesh-wide: a .pub file or the bare
        /// `RW…` line. Every node — including ones that join later —
        /// verifies releases against it, with no per-machine config
        #[arg(long, value_name = "FILE|KEY", conflicts_with_all = ["self_", "on"])]
        trust_key: Option<String>,
        /// What that key is, for whoever reads the list later
        #[arg(long, value_name = "TEXT", requires = "trust_key")]
        key_label: Option<String>,
        /// Withdraw trust in a release key, mesh-wide
        #[arg(long, value_name = "RW…", conflicts_with_all = ["self_", "on"])]
        untrust_key: Option<String>,
        /// List the release keys this mesh trusts, then stop
        #[arg(long, conflicts_with_all = ["self_", "on"])]
        keys: bool,
        /// Withdraw a plan: nodes it has not reached stop being eligible
        #[arg(long, value_name = "upg-…", conflicts_with_all = ["self_", "on"])]
        cancel: Option<String>,
        /// Only report the installed vs latest version; change nothing
        #[arg(long)]
        check: bool,
        /// Put back the binary the last `--self` upgrade replaced
        /// (`<exe>.prev`), and restart the daemon onto it. A node also does
        /// this by itself when a new version fails to start three times
        #[arg(long, conflicts_with_all = ["on", "check", "version", "from_file", "publish"])]
        rollback: bool,
        /// Install this release tag instead of the latest (e.g. v0.2.0).
        /// With --on this is the version being rolled out, and required.
        #[arg(long, visible_alias = "to", value_name = "TAG")]
        version: Option<String>,
        /// Install this already-fetched binary instead of downloading one.
        /// The mesh delivered it; its signature is still verified
        #[arg(long, value_name = "FILE", requires = "minisig")]
        from_file: Option<String>,
        /// The .minisig document for --from-file
        #[arg(long, value_name = "FILE")]
        minisig: Option<String>,
        /// The hardware co-signature for --from-file
        ///
        /// `ssh-keygen -Y sign -n meshware-release`. Required only where
        /// the mesh trusts an SSH-form release key; the daemon passes the
        /// mesh's own when it starts this process for a fleet rollout.
        ///
        /// Not `--cosign`: that is the publishing flag above, and two
        /// arguments with one name made clap hand this one to the other —
        /// which conflicts with `--self`, so every co-signed fleet rollout
        /// failed before it verified anything.
        #[arg(long = "from-cosign", value_name = "FILE", requires = "from_file")]
        cosign: Option<String>,
        /// Release base URL to fetch from (default: `[release] base`, else
        /// the env seam, else the project's GitHub releases)
        #[arg(long, value_name = "URL")]
        release_base: Option<String>,
        /// Base64 minisign public key to verify against (repeatable;
        /// default: the keys this mesh trusts, else `[release] pubkey`,
        /// else the key embedded in this binary)
        #[arg(long = "release-key", value_name = "KEY")]
        release_key: Vec<String>,
        /// Install and stop; leave the running daemon alone. A fleet
        /// rollout uses this — the daemon restarts itself
        #[arg(long)]
        no_drain: bool,
        /// Pass --move to the drain: re-dispatch queued jobs before exiting
        #[arg(long = "move")]
        move_pending: bool,
        /// With --on: re-install even on nodes already reporting the
        /// version. For a binary replaced by hand, an install that
        /// half-finished, or a node whose version no longer describes what
        /// it is running — every node still verifies the release signature
        /// before it swaps anything
        #[arg(long, requires = "on")]
        force: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

/// A sink record with nothing in it but a name and a kind.
///
/// Used where only the *shape* matters — printing DDL — so nothing has to
/// round-trip a credential through the CLI to answer a question about
/// column types.
fn blank_sink(name: &str, kind: mesh_proto::SinkKind) -> mesh_proto::Sink {
    mesh_proto::Sink {
        name: name.to_string(),
        kind,
        url: String::new(),
        database: String::new(),
        credential: String::new(),
        read_credential: String::new(),
        tables: Default::default(),
        records: Default::default(),
        payloads: Default::default(),
        on_failure: Default::default(),
        run_on: String::new(),
        enabled: true,
        description: String::new(),
        created_by: mesh_proto::UserId([0u8; 32]),
        created_at_micros: 0,
        signature: Vec::new(),
    }
}

/// A secret's scope, as flags (SEC-7).
#[derive(clap::Args, Debug, Default)]
struct ScopeArgs {
    /// Only nodes matching this selector hold it (`run --on` grammar)
    #[arg(long, value_name = "SELECTOR")]
    on: Option<String>,
    /// Only these users may use it in a job (repeatable; username or id)
    #[arg(long = "user", value_name = "USER")]
    users: Vec<String>,
    /// …or anyone holding one of these roles (repeatable)
    #[arg(long = "role", value_name = "ROLE")]
    roles: Vec<String>,
}

impl ScopeArgs {
    fn is_set(&self) -> bool {
        self.on.is_some() || !self.users.is_empty() || !self.roles.is_empty()
    }
}

/// Sign and publish a secret's scope.
async fn publish_scope(ctx: &Ctx, name: &str, args: &ScopeArgs) -> Result<()> {
    let mut users = Vec::new();
    for u in &args.users {
        users.push(resolve_user_id(ctx, u).await?);
    }
    let mut roles = Vec::new();
    for r in &args.roles {
        roles.push(mesh_proto::Role::parse(r).ok_or_else(|| {
            anyhow::anyhow!("{r:?} is not a role (admin, operator, developer, auditor, viewer)")
        })?);
    }
    let mut scope = mesh_proto::SecretScope {
        name: name.to_string(),
        nodes: args.on.clone().unwrap_or_default().trim().to_string(),
        users,
        roles,
        by: ctx.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    scope.signature = ctx.sign(&scope.signing_bytes())?;
    let resp = ctx
        .ipc(IpcRequest::SecretScopeSet {
            scope: Box::new(scope),
        })
        .await?;
    let IpcResponse::Ok = resp else {
        return unexpected(resp).map(|_| ());
    };
    Ok(())
}

/// The flags every generation-bumping deployment verb shares (N1 P1).
///
/// `put`, `rollout` and `rollback` are the same operation seen from three
/// angles — each re-signs the record and each produces a roll — so each
/// follows it, and each takes the same two flags for not following it.
#[derive(Args, Clone)]
struct RolloutWatch {
    /// Publish and return at once, without watching the rollout
    #[arg(long)]
    detach: bool,
    /// Stop **watching** if nothing reaches healthy for this long. The
    /// clock resets every time an instance goes healthy, so this bounds a
    /// stall, not the rollout.
    ///
    /// Named for what it bounds, and not `--timeout`, for two reasons. The
    /// global `--timeout <SECS>` is flattened into every subcommand and
    /// already owns that name. And it would be the wrong word regardless:
    /// the deadline that actually *fails* a rollout is the deployment's own
    /// `progress_deadline`, which runs on the elected reconciler whether or
    /// not anybody is watching. This only ever ends the watch.
    #[arg(long, value_name = "DURATION", default_value = "10m")]
    watch_timeout: String,
}

impl RolloutWatch {
    fn deadline(&self) -> Result<std::time::Duration> {
        let d = humantime::parse_duration(&self.watch_timeout).with_context(|| {
            format!(
                "--watch-timeout takes a duration (10m, 2h), not {:?}",
                self.watch_timeout
            )
        })?;
        if d.as_secs() == 0 {
            bail!("--watch-timeout 0 would give up before the first poll; use --detach");
        }
        Ok(d)
    }
}

fn resolve_config(global: &GlobalArgs, overrides: Overrides) -> Result<NodeConfig> {
    let merged = Overrides {
        data_dir: global.data_dir.clone().or(overrides.data_dir),
        ..overrides
    };
    NodeConfig::load(global.config.as_deref(), &merged).context("resolving configuration")
}

/// Resolved CLI context: the daemon to talk to + the user key that signs
/// every request (MASTERPLAN §7.1).
struct Ctx {
    config: NodeConfig,
    user: Keypair,
    /// The homes discovery looked in and found nothing listening in, so a
    /// failed request can say where it looked. Empty when the operator
    /// named a home, or when discovery found one.
    searched: Vec<PathBuf>,
    /// `--timeout`, in seconds; 0 waits forever. Unset means "pick one
    /// from the verb" — see [`Ctx::deadline_secs`] (**M-14**).
    timeout: Option<u64>,
    /// The IPC connection, kept open across requests (**M-15**).
    ///
    /// The daemon's accept loop has always read frames in a loop, so one
    /// connection can carry a whole invocation — but the client opened a
    /// fresh `UnixStream` per request. Twenty-nine verbs make more than one
    /// call, and `top --follow` and `jobs wait` re-connected every second:
    /// ~86 000 connections a day for one watcher, each costing the daemon
    /// an accept, a task spawn and a full setup.
    ///
    /// Only ever holds a connection known to be *idle* — see
    /// [`Ctx::ipc`] — and never a streaming one, which takes the socket
    /// over entirely.
    conn: tokio::sync::Mutex<Option<tokio::net::UnixStream>>,
}

/// [`mesh_core::layout::Layout::adopt_director_node`], for the verbs that
/// ask "is this machine in a mesh?" of the home before any node has started
/// to answer it — `join` and `pair` — and so would otherwise answer no on a
/// machine whose mesh Director used to run, and join a second one.
fn adopt_before_asking(
    global: &GlobalArgs,
    overrides: &Overrides,
    config: NodeConfig,
) -> Result<NodeConfig> {
    let moved = config.layout().adopt_director_node()?;
    if moved.is_empty() {
        return Ok(config);
    }
    eprintln!(
        "This machine's mesh used to run inside Director; its node now lives in {}.",
        config.data_dir.display()
    );
    resolve_config(global, overrides.clone())
}

/// What to do about a machine whose node is not running, said about *this*
/// machine rather than about machines in general.
///
/// "Start one with `meshnode up`" was the old advice, and on a machine
/// whose mesh Director had been running it was exactly wrong: `up` found no
/// genesis in the home and founded a second mesh. What `up` does depends on
/// what is in the home, so the hint reads the home first.
fn not_running_hint(config: &NodeConfig) -> String {
    let layout = config.layout();
    if let Ok(Some(genesis)) = mesh_core::genesis::Genesis::load(&config.data_dir) {
        let start = if mesh_core::usernode::installed() {
            "systemctl --user start meshnode"
        } else {
            "meshnode up            in this terminal\n  meshnode up --detach   in the background, and at every login"
        };
        return format!(
            "This machine is in mesh '{}', but its node is not running. Start it:\n  {start}",
            genesis.body.mesh_name
        );
    }
    if mesh_core::layout::Layout::new(layout.director_dir())
        .genesis_file()
        .exists()
    {
        return format!(
            "This machine's mesh is the one Director used to run, in {}. `meshnode up` \
             moves it into {} and runs it — the same mesh, not a new one.",
            layout.director_dir().display(),
            layout.root().display()
        );
    }
    "This machine is not in a mesh yet. `meshnode up` founds one; `meshnode pair` or \
     `meshnode join <token>` joins one."
        .to_string()
}

/// Which user key file the `identity protect|unlock|lock|status` verbs act on.
///
/// `resolve_config` and nothing else: no node discovery, no socket, no key
/// load. Discovery would pick the home of whichever daemon happens to be
/// running, and "protect my key" has to mean the file named by `--data-dir`
/// and `--user-identity` — the ones the person typed.
fn identity_key_path(global: &GlobalArgs) -> Result<PathBuf> {
    let config = resolve_config(global, Overrides::default())?;
    Ok(identity_protect::target_path(
        &config.data_dir,
        global.user_identity.as_deref(),
    ))
}

impl Ctx {
    /// Sign with the caller's user key, whether it is on this disk or on a
    /// card (Y2/Y3).
    ///
    /// Every client-side signature in this binary goes through here rather
    /// than through `Keypair::sign`, and that is the whole of the hardware
    /// story on the CLI side: a hardware key returns the same 64 bytes, and
    /// a hardware key whose token is unplugged returns an error naming the
    /// token instead of panicking or — much worse — quietly signing with
    /// something else (landmine 3).
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        Ok(self.user.try_sign(msg)?.to_vec())
    }

    fn new(global: &GlobalArgs) -> Result<Ctx> {
        let config = resolve_config(global, Overrides::default())?;
        // Naming a home — `--data-dir`, `$MESHWARE_HOME`, `$MESHWARE_DATA_DIR`,
        // or a `--config` that fixes one — means *that* home, including the
        // failure of talking to a node that is not in it. Discovery is for
        // the bare `meshnode status` typed on a workstation, where the
        // person has no reason to know which home the machine's node runs
        // from: a packaged node's, or the node an older Director still runs
        // inside itself.
        let named_home = global.data_dir.is_some()
            || global.config.is_some()
            || std::env::var_os(mesh_core::layout::LEGACY_DATA_ENV).is_some_and(|v| !v.is_empty());
        let (config, searched) = if named_home {
            (config, Vec::new())
        } else {
            match mesh_core::ipc::find_node_home(&config.data_dir) {
                Ok(home) if home == config.data_dir => (config, Vec::new()),
                // A different home: reload through it, so the config file,
                // the socket *and* the user key all come from the node we
                // are about to talk to rather than from three places.
                Ok(home) => (
                    resolve_config(
                        global,
                        Overrides {
                            data_dir: Some(home),
                            ..Default::default()
                        },
                    )?,
                    Vec::new(),
                ),
                Err(searched) => (config, searched),
            }
        };
        let user =
            mesh_core::identity::load_cli_user(&config.data_dir, global.user_identity.as_deref())
                .context("loading user identity")?;
        Ok(Ctx {
            config,
            user,
            searched,
            timeout: global.timeout,
            conn: tokio::sync::Mutex::new(None),
        })
    }

    /// The socket every request goes to.
    ///
    /// `$MESHWARE_SOCKET` overrides the derivation outright: the escape
    /// hatch for a node whose home this machine cannot guess, and already
    /// what the executor sets for a job that shells back into the CLI
    /// (`mesh_core::runtime::CLIENT_SOCKET_ENV`).
    fn socket(&self) -> PathBuf {
        std::env::var_os(mesh_core::runtime::CLIENT_SOCKET_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.socket_path())
    }

    /// Turn "nothing at this path" into "nothing anywhere I looked".
    ///
    /// The derived path is a hash no one recognises, so on its own it reads
    /// as a bug rather than as "no node is running". Naming the homes makes
    /// the next move obvious.
    fn unreachable(&self, e: impl std::fmt::Display) -> anyhow::Error {
        if self.searched.is_empty() {
            // A named home: the hint only when nothing is there to answer.
            // This also wraps a node that answered and then failed, or one
            // that refused this account, and "its node is not running" would
            // send either of those people the wrong way.
            let silent = matches!(
                std::os::unix::net::UnixStream::connect(self.socket()),
                Err(ref err) if err.kind() != std::io::ErrorKind::PermissionDenied
            );
            return if silent {
                anyhow!("{e}\n\n{}", not_running_hint(&self.config))
            } else {
                anyhow!("{e}")
            };
        }
        let mut msg = format!("{e}\n\nNo meshware node is listening on this machine. Looked in:");
        for home in &self.searched {
            msg.push_str(&format!("\n  {}", home.display()));
        }
        msg.push_str("\n\n");
        msg.push_str(&not_running_hint(&self.config));
        msg.push_str("\nA node in some other home: `meshnode --data-dir <DIR>`.");
        anyhow!("{msg}")
    }

    fn user_id(&self) -> UserId {
        self.user.user_id()
    }

    /// Signed single-response request; Error/Denied become CLI errors.
    ///
    /// **Bounded** (**M-14**). `ipc::request` connects, writes and then
    /// waits for a frame with no deadline of its own, so a daemon that had
    /// accepted the socket and then stalled — mid-fsync, deadlocked,
    /// swapping — hung the command with no output and no way to tell it
    /// from slow work. Every verb inherited that, and so did every script
    /// that shells out to one.
    async fn ipc(&self, req: IpcRequest) -> Result<IpcResponse> {
        let name = request_name(&req);
        let secs = self.deadline_secs(&req);
        let resp = self
            .deadline(name, secs, self.send(req))
            .await?
            .map_err(|e| self.unreachable(e))?;
        match &resp {
            IpcResponse::Error { message } => bail!("{message}"),
            IpcResponse::Denied { message } => bail!(
                "permission denied: {message} (you are {})",
                self.user_id().to_display_string()
            ),
            _ => Ok(resp),
        }
    }

    /// Signed streaming request; returns the open stream to consume.
    ///
    /// The deadline covers **opening** the stream only. A follow is
    /// supposed to stay quiet for hours — `jobs logs --follow` on an idle
    /// job, `top --follow` between ticks — so a deadline on the frames
    /// themselves would cancel exactly the thing the caller asked for.
    async fn stream(&self, req: IpcRequest) -> Result<tokio::net::UnixStream> {
        let name = request_name(&req);
        let secs = self.deadline_secs(&req);
        self.deadline(
            name,
            secs,
            ipc::request_stream(&self.socket(), &self.user, req),
        )
        .await?
        .map_err(|e| self.unreachable(e))
    }

    /// One signed request, on a connection reused where it is safe to
    /// (**M-15**).
    ///
    /// **Retried only when the write failed.** A write to a socket the
    /// daemon has closed fails immediately and proves the request was never
    /// received, so reopening and resending is safe even for a submission.
    /// A write that *succeeded* followed by a failed read proves nothing —
    /// the daemon may have run the job and lost the reply — so that is
    /// surfaced as an error rather than silently repeated.
    ///
    /// **Cancel-safe, because `--timeout` cancels it** (ENG-27). `deadline`
    /// races this future against a timer and drops it when the timer wins,
    /// which can happen after the request was written and before the reply
    /// was read. The pooled connection then held an unread reply, and the
    /// *next* command wrote onto it and read that stale reply as its own
    /// answer — `meshnode jobs` printing the answer to the `status` that
    /// timed out a moment earlier, with nothing anywhere saying so.
    ///
    /// So the connection is **taken** out of the pool for the round trip
    /// and put back only once a reply has been read whole. Cancelled
    /// anywhere in between, it is dropped with the future, and the next
    /// command opens a fresh one — which costs a connect and cannot be
    /// wrong.
    async fn send(
        &self,
        req: IpcRequest,
    ) -> std::result::Result<IpcResponse, mesh_core::CoreError> {
        let env = self.user.sign_request(&req)?;
        let mut held = self.conn.lock().await;
        if let Some(mut stream) = held.take() {
            // On a write error the request was never delivered: the daemon
            // restarted, or the socket aged out. Fall through and open a
            // fresh one.
            if let Ok(()) = mesh_proto::write_frame(&mut stream, &env).await {
                let resp = mesh_proto::read_frame::<_, IpcResponse>(&mut stream).await;
                if resp.is_ok() {
                    // A whole reply was read: the connection is in a
                    // known state and may be reused.
                    *held = Some(stream);
                }
                return Ok(resp?);
            }
        }
        let mut stream = ipc::connect(&self.socket()).await?;
        mesh_proto::write_frame(&mut stream, &env).await?;
        let resp: IpcResponse = mesh_proto::read_frame(&mut stream).await?;
        // **Pool it only if it is quiet.** A variant that streams answers
        // with more than one frame, and keeping such a connection would
        // leave those frames to be read as the *next* request's reply.
        // Nothing on this path should stream — `Ctx::stream` exists for
        // that — but the check costs a millisecond and removes a whole
        // class of desynchronisation rather than relying on a list of
        // variants staying correct.
        if tokio::time::timeout(std::time::Duration::from_millis(1), stream.readable())
            .await
            .is_err()
        {
            *held = Some(stream);
        }
        Ok(resp)
    }

    /// How long to wait for *this* verb (**M-14**).
    ///
    /// An explicit `--timeout` is obeyed exactly, including `0` for "wait
    /// forever". Otherwise the deadline comes from the request, because a
    /// flat one cannot be both useful and safe: thirty seconds is the right
    /// answer for `status` and would abort a fleet upgrade, a benchmark or
    /// a repository ingest mid-flight — turning a bug about hanging into a
    /// bug about giving up.
    ///
    /// The slow verbs are still *bounded*. A wedged daemon is the thing
    /// this exists to catch, and it should be caught eventually even for
    /// work that legitimately takes a while.
    fn deadline_secs(&self, req: &IpcRequest) -> u64 {
        if let Some(explicit) = self.timeout {
            return explicit;
        }
        mesh_core::ipc::deadline_for(req)
    }

    /// Run `fut` under `--timeout`, with a nudge to stderr if it is slow.
    ///
    /// The notice matters as much as the deadline: thirty seconds of a
    /// silent terminal reads as a hung command, so after two seconds it
    /// says what it is waiting for. On a TTY only — a script's stderr is
    /// somebody's log file.
    async fn deadline<T>(
        &self,
        what: &str,
        secs: u64,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T> {
        tokio::pin!(fut);
        let notice = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::pin!(notice);
        let started = std::time::Instant::now();
        loop {
            if secs == 0 {
                // Explicitly unbounded, for the person who asked for it.
                return Ok(fut.await);
            }
            tokio::select! {
                out = &mut fut => return Ok(out),
                () = &mut notice, if !notice.is_elapsed() => {
                    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                        eprintln!("waiting for the node to answer {what}… (--timeout {secs}s)");
                    }
                }
                () = tokio::time::sleep(
                    std::time::Duration::from_secs(secs).saturating_sub(started.elapsed()),
                ) => {
                    bail!(
                        "the node accepted the connection but did not answer {what} \
                         within {secs}s. It may be wedged — try `meshnode doctor` on \
                         that machine, or raise --timeout (0 waits forever)."
                    );
                }
            }
        }
    }
}

/// The CLI's half of `farm::Submitter`: a daemon socket, pumped into a
/// channel so the wrapper reads frames the same way a test does.
impl meshware_node::farm::Submitter for Ctx {
    fn submit(
        &self,
        req: IpcRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<tokio::sync::mpsc::Receiver<IpcResponse>>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let mut stream = self.stream(req).await?;
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream).await {
                    if tx.send(frame).await.is_err() {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }
}

fn parse_env(pairs: &[String]) -> Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|p| {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--env must be KEY=VALUE, got {p:?}"))?;
            // Resource requirements have their own flags, so there is one
            // way to declare them rather than two that can disagree.
            if mesh_proto::is_reserved_env(k) {
                anyhow::bail!(
                    "{k} is a reserved job-environment key: use --cpus/--mem/--disk \
                     for requirements and --gang/--gang-placement for a gang, so there \
                     is one way to declare each rather than two that can disagree"
                );
            }
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Split `meshnode parallel`'s trailing words into a command and an item
/// list, GNU-parallel style: everything before `:::` is the command,
/// everything after is a literal list. With no `:::`, items come from
/// `--arg-file` (`-` = stdin) or, failing that, from stdin — which is what
/// makes `seq 1 1000 | meshnode parallel 'process {}'` work.
fn parse_parallel_input(
    rest: Vec<String>,
    arg_file: Option<&str>,
) -> Result<(String, Vec<String>)> {
    Ok((
        parse_parallel_command(&rest)?,
        parallel_items(rest, arg_file)?,
    ))
}

/// The command alone: everything before `:::`.
fn parse_parallel_command(rest: &[String]) -> Result<String> {
    let words = match rest.iter().position(|w| w == ":::") {
        Some(at) => &rest[..at],
        None => rest,
    };
    if words.is_empty() {
        anyhow::bail!("no command given (try: meshnode parallel 'echo {{}}' ::: a b c)");
    }
    Ok(words.join(" "))
}

/// The item list alone: everything after `:::`, or `--arg-file`, or stdin.
/// Split out because `--service` takes no command — the script is the work,
/// so every trailing word is an item.
fn parallel_items(rest: Vec<String>, arg_file: Option<&str>) -> Result<Vec<String>> {
    let listed = rest
        .iter()
        .position(|w| w == ":::")
        .map(|at| rest[at + 1..].to_vec());
    let items = match (listed, arg_file) {
        (Some(items), None) => items,
        (Some(_), Some(_)) => {
            anyhow::bail!("pass items with `:::` or --arg-file, not both");
        }
        (None, Some("-")) | (None, None) => read_items(std::io::stdin().lock())?,
        (None, Some(path)) => {
            let file = std::fs::File::open(path)
                .map_err(|e| anyhow::anyhow!("cannot read --arg-file {path}: {e}"))?;
            read_items(std::io::BufReader::new(file))?
        }
    };
    if items.is_empty() {
        anyhow::bail!("no items to run: pass `::: item...`, --arg-file FILE, or pipe them in");
    }
    Ok(items)
}

/// One item per line; blank lines are skipped, as GNU parallel does.
fn read_items(source: impl std::io::BufRead) -> Result<Vec<String>> {
    let mut items = Vec::new();
    for line in source.lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            items.push(trimmed.to_string());
        }
    }
    Ok(items)
}

/// What a stranger sees the first time this machine founds a mesh
/// (C0-C9 §7.3 P3, MASTERPLAN §3).
///
/// **The two-minute rule is about a person, not a process.** The cold-start
/// timer says the software gets from nothing to scheduling in a fraction of
/// a second; none of that helps somebody who has just watched four lines of
/// tracing output scroll past and does not know what to type next. Before
/// this, `meshnode up` told them their node id and nothing else — the
/// installer's parting advice was the only place the next step existed, and
/// it is gone from the scrollback by the time they need it.
///
/// So the banner is printed **only on the run that creates the mesh**. On
/// every subsequent start it would be noise in a log, and a daemon that
/// greets its operator on every restart is a daemon people stop reading.
fn first_run_banner(config: &NodeConfig) {
    // stderr, beside the tracing output it introduces, so `meshnode up`
    // piped into something still pipes only what it always did.
    eprintln!();
    eprintln!("  Welcome. This machine is founding a new mesh.");
    eprintln!();
    eprintln!("    mesh       {}", config.mesh_name);
    eprintln!("    node       {}", config.node_name);
    eprintln!("    listening  {}", config.listen);
    eprintln!("    data       {}", config.data_dir.display());
    eprintln!();
    eprintln!("  Next, in another terminal:");
    eprintln!();
    eprintln!("    meshnode status                  what this node thinks is going on");
    eprintln!("    meshnode run 'uname -a'          run something, streamed live");
    eprintln!("    meshnode invite create           a token for the second machine");
    eprintln!();
    eprintln!(
        "{}",
        term::err().prose("  On that second machine, paste the token into `meshnode join <token>`")
    );
    eprintln!(
        "{}",
        term::err().prose("  and then run `meshnode up`. On the same LAN you can skip the token")
    );
    eprintln!(
        "{}",
        term::err()
            .prose("  entirely: `meshnode join --pair` there, `meshnode invite approve` here,")
    );
    eprintln!("  and confirm the 6-digit code both screens show.");
    eprintln!();
    eprintln!("  There is no server to install. This is the whole of it.");
    eprintln!();
}

/// The greeting for the one start that moves Director's old node into this
/// home (`Layout::adopt_director_node`). Said once, on stderr like the
/// first-run banner, because files moving is something a person should hear
/// about from the program that moved them.
fn adopted_banner(config: &NodeConfig, moved: &[String]) {
    eprintln!();
    eprintln!("  This machine's mesh used to run inside Director. It runs here now.");
    eprintln!();
    eprintln!("    node       {}", config.node_name);
    eprintln!("    data       {}", config.data_dir.display());
    eprintln!();
    eprintln!("  Moved into this home:");
    for line in moved {
        eprintln!("    {line}");
    }
    eprintln!();
    eprintln!(
        "{}",
        term::err().prose("  Director and `meshnode` are both views onto this node now: quitting")
    );
    eprintln!("  one changes nothing for the other.");
    eprintln!();
}

/// `meshnode up --detach`: this machine's node, in the background.
///
/// The unit is [`mesh_core::usernode`]'s — the same one Director installs
/// and every machine added over SSH gets — so a node started here and a
/// node started from Director are the same node, and either frontend can
/// stop, restart or read the other's.
async fn up_detached(config: &NodeConfig) -> Result<i32> {
    let service_home = mesh_core::usernode::home().context("no home directory")?;
    let same_home = match (
        std::fs::canonicalize(&config.data_dir),
        std::fs::canonicalize(&service_home),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => config.data_dir == service_home,
    };
    if !same_home {
        bail!(
            "--detach runs this account's node, whose home is {}; this one's is {}. Run \
             `meshnode up` for that home under whatever should keep it running.",
            service_home.display(),
            config.data_dir.display()
        );
    }
    let socket = config.socket_path();
    if ipc::connect(&socket).await.is_ok() {
        println!("This machine's node is already running.");
        return Ok(0);
    }
    let exe = std::env::current_exe().context("finding this binary")?;
    let started = if mesh_core::usernode::installed() {
        mesh_core::usernode::start(&exe, &config.data_dir)?
    } else {
        mesh_core::usernode::install(&exe)?
    };
    // `Type=notify` makes `systemctl start` wait for the node to say it is
    // ready, so the service case is normally answered at once; a detached
    // process has no such handshake and is polled.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while ipc::connect(&socket).await.is_err() {
        if std::time::Instant::now() >= deadline {
            bail!(
                "the node was started but is not answering on {} yet. {}",
                socket.display(),
                match &started {
                    mesh_core::usernode::Started::Service =>
                        "`journalctl --user -u meshnode` says why.".to_string(),
                    mesh_core::usernode::Started::Process { log, .. } =>
                        format!("Its output is in {}.", log.display()),
                }
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    match started {
        mesh_core::usernode::Started::Service => {
            println!(
                "This machine's node is running in the background, as a systemd user service."
            );
            println!();
            println!("  meshnode status                    see it");
            println!("  journalctl --user -u meshnode -f   its log");
            println!("  systemctl --user stop meshnode     stop it");
            println!();
            println!("It starts again at every login. To keep it running after logout as well:");
            println!("  loginctl enable-linger $USER");
        }
        mesh_core::usernode::Started::Process { pid, log } => {
            println!("This machine's node is running in the background (pid {pid}).");
            println!(
                "There is no systemd user manager here, so it will not come back after a \
                 restart by itself. Its output: {}",
                log.display()
            );
        }
    }
    Ok(0)
}

/// `--gang/--gang-placement/--gang-restart` as reserved environment
/// (C0-C9 §8.2).
///
/// The refusals here are the ones worth making early, on the client, where
/// the message can name the flag the user typed:
///
/// - **A gang of one is not a gang.** It is a job, and a two-phase
///   handshake bought for nothing is a slower job with more ways to fail.
/// - **`--gang` and `--fanout` are different questions.** A fan-out asks
///   for *one job per matching node, independently*; a gang asks for *N
///   nodes at once, or none*. Accepting both would have to silently pick
///   one, and the user who typed both does not know which they got.
/// - **`--gang-placement` is spelled, not guessed.** A typo that fell back
///   to the default would quietly scatter a training run that was asked to
///   be packed.
fn gang_env(
    gang: Option<u32>,
    placement: Option<&str>,
    restart: bool,
    fanout: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let Some(size) = gang else {
        if placement.is_some() || restart {
            anyhow::bail!("--gang-placement and --gang-restart need --gang N");
        }
        return Ok(Vec::new());
    };
    if size < 2 {
        anyhow::bail!(
            "--gang {size} is not a gang: a gang is an all-or-nothing reservation \
             over two or more nodes. Drop the flag to run one job."
        );
    }
    if fanout.is_some() {
        anyhow::bail!(
            "--gang and --fanout ask different questions: --fanout runs one \
             independent job per matching node, --gang runs N nodes at once or \
             none. Pick one."
        );
    }
    let mut env = vec![(mesh_proto::REQ_GANG.to_string(), size.to_string())];
    if let Some(p) = placement {
        let parsed = mesh_proto::GangPlacement::parse(p).ok_or_else(|| {
            anyhow::anyhow!("--gang-placement must be `pack` or `spread`, not {p:?}")
        })?;
        env.push((
            mesh_proto::REQ_GANG_PLACEMENT.to_string(),
            parsed.as_str().to_string(),
        ));
    }
    if restart {
        env.push((mesh_proto::REQ_GANG_RESTART.to_string(), "1".to_string()));
    }
    Ok(env)
}

/// `--cpus/--mem/--disk` as declared task requirements (MASTERPLAN M9 §5.3).
/// `--cacheable` / `--cache-outputs` as reserved job environment (C7 §11.2).
///
/// `--explain-key` is a *mode*, not an environment key — it changes what
/// the daemon does with the submission rather than what the submission is —
/// but it is validated here because it is meaningless without
/// `--cacheable` and saying so at the CLI beats a round trip to be told.
fn cache_env(
    cacheable: bool,
    outputs: Option<&str>,
    explain_key: bool,
) -> Result<Vec<(String, String)>> {
    if !cacheable {
        if explain_key {
            bail!(
                "--explain-key needs --cacheable: an action key is only meaningful for work \
                 whose author has claimed it is a pure function of its inputs"
            );
        }
        if outputs.is_some() {
            bail!("--cache-outputs needs --cacheable");
        }
        return Ok(Vec::new());
    }
    let mut env = vec![(mesh_proto::REQ_CACHE.to_string(), "1".to_string())];
    if let Some(globs) = outputs {
        let cleaned: Vec<&str> = globs
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .collect();
        if cleaned.is_empty() {
            bail!("--cache-outputs was given no globs");
        }
        // An absolute or escaping glob is refused here rather than on the
        // executor: a cached output that could be written outside the job
        // directory would make a cache hit a remote write, and the earliest
        // possible "no" is the right one.
        for glob in &cleaned {
            if glob.starts_with('/') || glob.split('/').any(|c| c == "..") {
                bail!("--cache-outputs globs are relative to the working directory: {glob}");
            }
        }
        env.push((mesh_proto::REQ_CACHE_OUTPUTS.to_string(), cleaned.join(",")));
    }
    Ok(env)
}

/// `--port http:9095 --port admin:8080!` as the reserved port request
/// (N2 P2).
///
/// One key, the same one `[deployment.network]` produces and the same one
/// a service's own `--env` can set — so a port request has exactly one
/// encoding however it was asked for, and the executor has one thing to
/// read.
///
/// Validated **here**, on the client, where the error can name the flag
/// that was typed. The parser in `mesh-proto` drops junk silently because
/// it is reading an environment a job may have been given by anything; a
/// person typing `--port` deserves to be told.
fn parse_volume_request(specs: &[String]) -> Result<Vec<(String, String)>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    if specs.len() > mesh_proto::MAX_VOLUMES_PER_JOB {
        bail!(
            "at most {} --volume claims (got {})",
            mesh_proto::MAX_VOLUMES_PER_JOB,
            specs.len()
        );
    }
    let mut out: Vec<mesh_proto::VolumeRequest> = Vec::with_capacity(specs.len());
    for spec in specs {
        let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
        let (name, path, mode) = match parts.as_slice() {
            [name] => (*name, "", "rw"),
            // `data:ro` and `data:/mnt` are both two parts; a mode word is
            // the only thing the second can be if it parses as one.
            [name, second] if mesh_proto::volumes::Mode::parse(second).is_some() => {
                (*name, "", *second)
            }
            [name, path] => (*name, *path, "rw"),
            [name, path, mode] => (*name, *path, *mode),
            _ => bail!(
                concat!(
                    "--volume {:?} is NAME, NAME:MODE, or NAME:PATH:MODE — for example ",
                    "`data`, `data:ro`, or `data:/mnt/data:rw`",
                ),
                spec
            ),
        };
        if !mesh_proto::volumes::is_valid_volume_name(name) {
            bail!(
                concat!(
                    "--volume {:?}: {:?} must be 1-48 letters, digits, '-' or '_', ",
                    "starting with a letter or digit",
                ),
                spec,
                name
            );
        }
        let Some(mode) = mesh_proto::volumes::Mode::parse(mode) else {
            bail!("--volume {spec:?}: mode must be `rw` or `ro`");
        };
        if out.iter().any(|v| v.name == name) {
            bail!("--volume {name:?} is claimed twice; one claim cannot be two modes");
        }
        out.push(mesh_proto::VolumeRequest {
            name: name.to_string(),
            path: path.to_string(),
            mode,
        });
    }
    Ok(mesh_proto::VolumeRequests(out).to_env())
}

fn parse_port_request(ports: &[String]) -> Result<Vec<(String, String)>> {
    if ports.is_empty() {
        return Ok(Vec::new());
    }
    if ports.len() > mesh_proto::MAX_PORTS_PER_JOB {
        bail!(
            "at most {} --port labels (got {})",
            mesh_proto::MAX_PORTS_PER_JOB,
            ports.len()
        );
    }
    let joined = ports.join(",");
    let parsed = mesh_proto::PortRequests::parse(&joined);
    if parsed.len() != ports.len() {
        // Which one, rather than "one of these is wrong".
        for one in ports {
            if mesh_proto::PortRequests::parse(one).is_empty() {
                bail!(
                    "--port {one:?} is not LABEL:PORT — try `--port http:9095`, or \
                     `--port http:8080!` to demand that exact host port. A \
                     label is letters, digits, '-' or '_', and becomes \
                     MESHWARE_PORT_<LABEL>"
                );
            }
        }
        bail!("--port has a repeated label: {joined}");
    }
    Ok(parsed.to_env())
}

/// `--gpus/--vram-mb/--device/--gpu-share` as a device request (C5 §9.1).
///
/// The request travels as **one** reserved environment key holding JSON,
/// rather than as three more `MESHWARE_TASK_*` scalars, because this is the
/// requirement shape that will keep growing — MIG profiles, topology
/// constraints, fractional shares are all named as later work — and a
/// self-describing encoding is what lets an older executor read the fields
/// it knows instead of failing the decode.
fn parse_device_request(
    gpus: Option<u32>,
    vram_mb: Option<u64>,
    device: Option<&str>,
    share: bool,
) -> Result<Vec<(String, String)>> {
    let mut req = mesh_proto::DeviceRequest {
        count: gpus.unwrap_or(0),
        vram_mb: vram_mb.unwrap_or(0),
        share,
        ..Default::default()
    };
    if let Some(spec) = device {
        req.apply_selector(spec).map_err(|e| anyhow::anyhow!(e))?;
    }
    if req.is_unstated() {
        // `--gpu-share` alone asks for nothing to share. Refused on the
        // client, where the message can name the flag that was typed.
        if share {
            anyhow::bail!("--gpu-share needs a device request: add --gpus N or --device kind=…");
        }
        return Ok(Vec::new());
    }
    Ok(req.to_env())
}

fn parse_requirements(
    cpus: Option<u32>,
    mem: Option<&str>,
    disk: Option<&str>,
) -> Result<mesh_proto::Requirements> {
    Ok(mesh_proto::Requirements {
        cpus: cpus.unwrap_or(0),
        mem_mb: mem
            .map(|s| parse_size_mb("--mem", s))
            .transpose()?
            .unwrap_or(0),
        disk_mb: disk
            .map(|s| parse_size_mb("--disk", s))
            .transpose()?
            .unwrap_or(0),
    })
}

/// A human size (`512M`, `48G`, `2T`, `1.5G`, or a bare number of MB) as
/// megabytes. A trailing `B` is accepted and ignored, so `48GB` reads the
/// same as `48G`.
fn parse_size_mb(flag: &str, raw: &str) -> Result<u64> {
    let text = raw.trim();
    let text = text.strip_suffix(['B', 'b']).unwrap_or(text);
    let (digits, scale) = match text.strip_suffix(['K', 'k']) {
        Some(d) => (d, 1.0 / 1024.0),
        None => match text.strip_suffix(['M', 'm']) {
            Some(d) => (d, 1.0),
            None => match text.strip_suffix(['G', 'g']) {
                Some(d) => (d, 1024.0),
                None => match text.strip_suffix(['T', 't']) {
                    Some(d) => (d, 1024.0 * 1024.0),
                    None => (text, 1.0),
                },
            },
        },
    };
    let value: f64 = digits
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} must be a size like 512M or 48G, got {raw:?}"))?;
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("{flag} must be a non-negative size, got {raw:?}");
    }
    Ok((value * scale).round() as u64)
}

/// ANSI dim, for the one line of the greeting that is not a command.
///
/// clap renders help through `anstream`, which strips escapes when stdout
/// is not a terminal — so this is styling on a screen and plain text in a
/// pipe, a log or a `--help > file`.
const DIM: &str = "\x1b[2m";
const UNDIM: &str = "\x1b[0m";

/// The greeting's last two blocks: where to start, and how many commands
/// the eight-verb list is not showing (L1 P1).
///
/// The count is computed, never written down. A hard-coded "50 more" would
/// be wrong the first time somebody added a verb, and a number this product
/// prints has to be checkable — which is the same rule the totals strip and
/// the cache report follow.
fn greeting_footer(hidden: usize) -> String {
    format!(
        "Start here:\n  \
         meshnode demo    three nodes on this machine, a real fan-out, gone when you stop it\n  \
         meshnode up      start this node — on first run it founds a mesh\n\n\
         {DIM}{hidden} more commands — `meshnode help --all`{UNDIM}"
    )
}

/// The published mental model (MASTERPLAN §1), in the order it publishes
/// them. These eight are the greeting; every other verb is `hide`.
const GREETED: [&str; 7] = ["pair", "run", "map", "shell", "top", "secrets", "drain"];

/// The eight first, in the MASTERPLAN's order; everything else after them,
/// alphabetically.
///
/// clap numbers subcommands in declaration order, which is the order the
/// enum happened to grow in and no order at all to a reader. The greeting
/// gets the published order; `help --all` gets the eight and then a list
/// somebody can scan.
fn ordered(cmd: clap::Command) -> clap::Command {
    cmd.mut_subcommands(|sc| {
        let order = GREETED
            .iter()
            .position(|v| *v == sc.get_name())
            .map_or(900, |i| i + 1);
        sc.display_order(order)
    })
}

/// The parser a person meets: eight verbs, then the footer.
fn cli_command() -> clap::Command {
    let cmd = styled(ordered(Cli::command()).long_version(long_version()));
    let hidden = cmd.get_subcommands().filter(|c| c.is_hide_set()).count();
    cmd.after_help(greeting_footer(hidden))
}

/// Help in the same colours as everything else: headings bold, what you
/// type (commands, flags) in cyan, and what you fill in (`<PATH>`) in a
/// quieter cyan. clap honours `NO_COLOR` and a pipe by itself; `--color`
/// has to be read ahead of it, because clap renders help while it is
/// still parsing the flag.
fn styled(cmd: clap::Command) -> clap::Command {
    use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
    const STYLES: Styles = Styles::styled()
        .header(Style::new().bold())
        .usage(Style::new().bold())
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Cyan.on_default())
        .valid(AnsiColor::Green.on_default().effects(Effects::BOLD))
        .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
        .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
        .context(Style::new().dimmed());
    let color = match term::choice_from_args() {
        term::ColorChoice::Auto => clap::ColorChoice::Auto,
        term::ColorChoice::Always => clap::ColorChoice::Always,
        term::ColorChoice::Never => clap::ColorChoice::Never,
    };
    cmd.styles(STYLES).color(color)
}

/// What `meshnode --version` prints (DOC-14): the version, then the
/// protocol, the target and the commit, one per line.
///
/// **The first line stays exactly `meshnode <version>`**, and `-V` prints
/// only that line: `upgrade --rollback`, the remote probe `adopt` runs and
/// the build scripts all read it. The commit comes from
/// `MESHWARE_BUILD_COMMIT`, which the build scripts set; a plain
/// `cargo build` says `unknown`.
fn long_version() -> &'static str {
    static LONG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LONG.get_or_init(|| {
        let libc = if cfg!(target_env = "musl") {
            "-musl"
        } else if cfg!(target_env = "gnu") {
            "-gnu"
        } else {
            ""
        };
        format!(
            "{}\nprotocol {}.{}\ntarget {}-{}{}\ncommit {}",
            env!("CARGO_PKG_VERSION"),
            mesh_proto::PROTOCOL_MAJOR,
            mesh_proto::PROTOCOL_MINOR,
            std::env::consts::ARCH,
            std::env::consts::OS,
            libc,
            option_env!("MESHWARE_BUILD_COMMIT")
                .filter(|c| !c.trim().is_empty())
                .unwrap_or("unknown"),
        )
    })
}

/// Which heading each verb sits under in `meshnode help --all`.
///
/// **68 verbs in one alphabetical list is the problem this solves**, and it
/// is a listing problem rather than a naming one. The review that prompted
/// this proposed folding ten nouns — `add`, `script`, `push`, `service`,
/// `definitions`, `bundles`, `content`, `sources`, `collectors`, `objects`
/// — into a `library` noun, because Director puts all ten on one screen
/// called Library and the two front ends therefore disagreed about how many
/// things exist.
///
/// They do not disagree any more, and no verb moved: the heading is called
/// **Library**. Renaming would have added a level (`meshnode library
/// scripts ls` for `meshnode script ls`), broken every script and doc
/// snippet that used the old spelling, and — for the release the aliases
/// lived through — made the surface *larger* rather than smaller. A
/// heading costs none of that and answers the same question.
///
/// A verb missing from this table gets no heading and lands under plain
/// `Commands:`, which is a visible reminder to file it rather than a
/// silent default.
const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Getting started",
        &[
            "up", "demo", "join", "pair", "adopt", "enrol", "login", "reset",
        ],
    ),
    (
        "Machines",
        &[
            "nodes", "top", "totals", "status", "doctor", "ping", "facts", "runtimes", "ports",
            "disk", "drain", "shed", "upgrade",
        ],
    ),
    (
        "Running work",
        &[
            "run", "parallel", "map", "jobs", "queue", "plan", "gangs", "objects", "cc", "cache",
            "results", "shell",
        ],
    ),
    (
        "Library — what the mesh distributes",
        &[
            "add",
            "script",
            "service",
            "definitions",
            "bundles",
            "content",
            "push",
            "sources",
            "collectors",
        ],
    ),
    (
        "Services that stay up",
        &["deployment", "schedule", "registry", "dns", "volumes"],
    ),
    (
        "People and access",
        &[
            "users",
            "whoami",
            "grants",
            "serviceaccounts",
            "invite",
            "sessions",
            "identity",
            "oidc",
            "audit",
            "secrets",
        ],
    ),
    (
        "This node",
        &[
            "config",
            "state",
            "retention",
            "history",
            "sinks",
            "backup",
            "restore",
            "support",
        ],
    ),
    ("Shell integration", &["help", "completion", "manpage"]),
];

/// Print every verb, under [`HELP_GROUPS`], for `meshnode help --all`.
///
/// Hand-rendered because clap has no heading for *subcommands* — only for
/// arguments — and an alphabetical wall of 68 is exactly what this is
/// fixing. Names and one-line descriptions come from the parser itself, so
/// a new verb appears here the moment it is declared; only its heading is
/// this table's to know.
fn print_grouped_help(cmd: &clap::Command) -> Result<()> {
    use std::io::Write as _;
    let out = std::io::stdout();
    let mut w = out.lock();
    writeln!(
        w,
        "{}\n",
        cmd.get_about().map(|a| a.to_string()).unwrap_or_default()
    )?;
    writeln!(w, "Usage: meshnode [OPTIONS] <COMMAND>")?;

    let subs: Vec<&clap::Command> = cmd.get_subcommands().collect();
    let width = subs
        .iter()
        .map(|s| s.get_name().len())
        .max()
        .unwrap_or(0)
        .max(8);
    let mut shown: Vec<&str> = Vec::new();

    for (heading, verbs) in HELP_GROUPS {
        let rows: Vec<&&clap::Command> = subs
            .iter()
            .filter(|s| verbs.contains(&s.get_name()))
            .collect();
        if rows.is_empty() {
            continue;
        }
        writeln!(w, "\n{heading}:")?;
        // The table's order, not the parser's: the table reads as a
        // sentence about the group and alphabetical does not.
        for verb in *verbs {
            let Some(sub) = rows.iter().find(|s| s.get_name() == *verb) else {
                continue;
            };
            shown.push(sub.get_name());
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            writeln!(w, "  {:<width$}  {about}", sub.get_name())?;
        }
    }

    // A verb nobody filed still has to appear, or this listing quietly
    // becomes less complete than the alphabetical one it replaced.
    let orphans: Vec<&&clap::Command> = subs
        .iter()
        .filter(|s| !shown.contains(&s.get_name()))
        .collect();
    if !orphans.is_empty() {
        writeln!(w, "\nCommands:")?;
        for sub in orphans {
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            writeln!(w, "  {:<width$}  {about}", sub.get_name())?;
        }
    }

    writeln!(
        w,
        "\n  meshnode help <command>   the full page for one verb\n  \
         meshnode --help          global options"
    )?;
    Ok(())
}

/// The same parser with every command visible again — what `help --all`,
/// `completion` and `manpage` are built from (D-L1.3).
///
/// This is load-bearing rather than tidy: `clap_complete` and `clap_mangen`
/// both *skip* hidden subcommands, so without this the greeting's promise
/// that nothing was removed would be false everywhere except the greeting.
fn full_command() -> clap::Command {
    fn unhide(cmd: clap::Command) -> clap::Command {
        cmd.hide(false).mut_subcommands(unhide)
    }
    styled(ordered(unhide(Cli::command())).long_version(long_version()))
}

/// Set once the command line says this process is `meshnode up`; the panic
/// hook in `main` reads it.
static IS_DAEMON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() {
    // First, before argument parsing, before any runtime: this process may
    // not be a CLI at all. A PTY session re-execs this binary as its child
    // so that `setsid` + `TIOCSCTTY` can run in an ordinary `main` instead
    // of an `unsafe` pre-exec closure (M17 §3.6, `sessions::pty_shim`).
    // If that is what we are, this never returns.
    #[cfg(unix)]
    mesh_core::sessions::pty_shim::try_run();
    // …and it may be the sandbox shim around a job: the same shape, for
    // the same reason (`mesh_core::sandbox`). Never returns if so.
    mesh_core::sandbox::try_run();
    // Before any thread or child exists: a capability a unit grants this
    // process — the retired mesh proxy's `proxy.conf`, on a machine not yet
    // re-deployed — stays with it and reaches nothing it runs.
    mesh_core::sandbox::drop_ambient_capabilities();

    // `meshnode … | head` used to end in a panic (R5 P3, finding #16):
    //
    //     panicked at library/std/src/io/stdio.rs: failed printing to
    //     stdout: Broken pipe (os error 32)
    //
    // `println!` unwraps its write, and a closed pipe is an error — so
    // piping any table into `head`, `less` or a `grep -m1` produced a
    // backtrace on stderr and a non-zero exit for a command that had done
    // exactly what was asked of it. Every script that reads this CLI's
    // output through a pipe had to work around it.
    //
    // The usual fix is to reset `SIGPIPE` to its default disposition, which
    // needs `unsafe` — and this workspace denies `unsafe_code` on purpose
    // (Cargo.toml §195). A panic hook is the safe equivalent: the write
    // still fails, and this turns the failure into the quiet exit the shell
    // was expecting rather than a stack trace. Anything that is *not* a
    // broken pipe reaches the default hook untouched.
    //
    // **CLI verbs only** (ENG-2). Installed before parsing, because clap's
    // own help output can hit the same closed pipe, but it stands down once
    // this process turns out to be the daemon: there a "Broken pipe" panic
    // is a task failing on a peer's or a client's socket, and turning it
    // into a quiet `exit(0)` took the whole node down and called it a
    // success.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if IS_DAEMON.load(std::sync::atomic::Ordering::Relaxed) {
            return default_hook(info);
        }
        let broken = |s: &str| s.contains("Broken pipe");
        let is_pipe = info
            .payload()
            .downcast_ref::<String>()
            .map(|s| broken(s))
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| broken(s)))
            .unwrap_or(false);
        if is_pipe {
            std::process::exit(0);
        }
        default_hook(info);
    }));

    // Everything this process creates is private by default (S-L1). The
    // packaged install already had this from the unit's `UMask=0077`; a
    // tarball or user install did not, and its data directory came out
    // world-readable — which is the whole replicated state, the audit trail
    // and every job's output. After the two shims, because a shim's job is
    // to exec the thing it was asked to exec and nothing else.
    #[cfg(unix)]
    mesh_core::layout::tighten_umask();

    let cli = Cli::from_arg_matches(&cli_command().get_matches()).unwrap_or_else(|e| e.exit());
    term::set_choice(cli.global.color);

    // Parse, and do nothing else (DOC-3). `scripts/check-docs.py commands`
    // runs every command line the documentation shows through the real
    // parser this way — unknown verbs, unknown flags, missing required
    // arguments and conflicts all fail exactly as they would for a reader
    // who pasted it — without a daemon, and without running anything.
    if std::env::var_os("MESHWARE_CHECK_ARGS").is_some_and(|v| !v.is_empty() && v != "0") {
        std::process::exit(0);
    }

    // Two runtimes, chosen by what the process is about to do.
    //
    // `#[tokio::main]` builds a worker thread per core for *every*
    // invocation, so `meshnode jobs` on a 64-core box spawned 64 threads to
    // write one frame to a socket and read one back. A CLI verb is one
    // request; it gets a single-threaded runtime, and starts measurably
    // faster for it. The daemon is the only thing here that is actually
    // concurrent, and it gets the full pool.
    if matches!(cli.command, Command::Up { detach: false, .. }) {
        IS_DAEMON.store(true, std::sync::atomic::Ordering::Relaxed);
        // Before anything this binary could fail at (ENG-12).
        upgrade_guard_start(&cli.global);
    }
    let runtime = match &cli.command {
        Command::Up { detach: false, .. } => match daemon_runtime(&cli) {
            Ok(rt) => rt,
            Err(e) => {
                upgrade_guard_abandon(&cli.global, &e);
                eprintln!("error: {e:#}");
                // Sizing the runtime is the first read of the config, so a
                // failure here is the config's (ENG-1).
                std::process::exit(mesh_core::daemon::exit_status::CONFIG);
            }
        },
        _ => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("error: starting the async runtime: {e}");
                std::process::exit(1);
            }
        },
    };

    // A session verb reads local stdin for the life of the session, and
    // `tokio::io::stdin` parks a **blocking-pool** thread to do it — a read
    // that only returns when the terminal closes. Dropping a runtime waits
    // for its blocking tasks, so the ordinary shutdown below would hang
    // forever after the far shell said `logout`. These verbs own the
    // process and have nothing left to unwind, so they exit directly.
    #[cfg(unix)]
    let holds_stdin = matches!(cli.command, Command::Shell { .. });
    #[cfg(not(unix))]
    let holds_stdin = false;

    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", term::error_line(&format!("{e:#}")));
            1
        }
    };
    if holds_stdin {
        std::process::exit(code);
    }
    // Drop the runtime before exiting so background tasks get their chance
    // to unwind — `process::exit` from inside `block_on` would not.
    drop(runtime);
    std::process::exit(code);
}

/// The daemon's runtime, sized from `[runtime]` (MASTERPLAN §12).
///
/// Threads are named so `top -H`, `perf` and a core dump all say which pool
/// a thread belongs to. That costs nothing and it is the difference between
/// diagnosing a stuck node in a minute and in an afternoon.
/// Where a pending upgrade is recorded, and the binary this process runs
/// from — settled without reading the config, which is one of the things a
/// new binary can fail at (ENG-12).
///
/// The home, not the config's `data_dir`: they are the same directory
/// unless the file moves `data_dir`, and a file this binary cannot read
/// cannot say where it moved to.
fn upgrade_guard_paths(global: &GlobalArgs) -> Option<(PathBuf, PathBuf)> {
    let home = mesh_core::layout::Layout::resolve(global.data_dir.as_deref());
    let exe = std::env::current_exe().ok()?;
    Some((home.root().to_path_buf(), exe))
}

/// Count this start against a pending upgrade, and hand over to the
/// previous binary once this one has had its chances (ENG-12).
fn upgrade_guard_start(global: &GlobalArgs) {
    use mesh_core::upgrade_guard::{self as guard, StartVerdict};
    let Some((home, exe)) = upgrade_guard_paths(global) else {
        return;
    };
    match guard::on_start(&home, env!("CARGO_PKG_VERSION"), &exe) {
        StartVerdict::Counted { starts } => eprintln!(
            "meshnode {}: start {starts} of {} since the upgrade; it is kept once healthy for {}s",
            env!("CARGO_PKG_VERSION"),
            guard::MAX_STARTS,
            guard::HEALTHY_FOR.as_secs()
        ),
        StartVerdict::RolledBack(record) => upgrade_guard_hand_over(&exe, &record),
        StartVerdict::RollbackFailed(why) => eprintln!("error: {why}"),
        StartVerdict::Nothing | StartVerdict::Stale => {}
    }
}

/// This binary refuses the configuration it was upgraded onto: roll back
/// now, since the refusal's exit status is one the service manager does
/// not retry (ENG-12).
fn upgrade_guard_abandon(global: &GlobalArgs, why: &dyn std::fmt::Display) {
    use mesh_core::upgrade_guard::{self as guard, StartVerdict};
    let Some((home, exe)) = upgrade_guard_paths(global) else {
        return;
    };
    match guard::abandon(&home, env!("CARGO_PKG_VERSION"), &exe) {
        StartVerdict::RolledBack(record) => {
            eprintln!("error: {why:#}");
            upgrade_guard_hand_over(&exe, &record);
        }
        StartVerdict::RollbackFailed(message) => eprintln!("error: {message}"),
        _ => {}
    }
}

/// Start the restored binary in this process's place: same pid, same
/// arguments, so a service manager sees one unit that recovered.
fn upgrade_guard_hand_over(
    exe: &std::path::Path,
    record: &mesh_core::upgrade_guard::RolledBack,
) -> ! {
    eprintln!(
        "error: meshnode {} did not become healthy ({} start(s)); restored {} and starting it. \
         The binary that failed is kept at {}.",
        record.failed,
        record.starts,
        record.restored,
        record.failed_exe.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec();
        eprintln!("error: could not start the restored binary: {err}");
    }
    std::process::exit(1)
}

fn daemon_runtime(cli: &Cli) -> Result<tokio::runtime::Runtime> {
    // Config is resolved twice on the `up` path — once here for thread
    // sizing, once inside the command for everything else. It is a file
    // read; the alternative is threading a half-built config through the
    // dispatcher to save it.
    let cfg = resolve_config(&cli.global, Overrides::default())?.runtime;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(cfg.worker_threads)
        .max_blocking_threads(cfg.max_blocking_threads)
        .thread_name("meshnode-worker")
        .enable_all();
    if let Some(stack) = cfg.thread_stack_bytes {
        builder.thread_stack_size(stack);
    }
    builder.build().context("starting the async runtime")
}

/// Returns the process exit code (job runs propagate the remote exit code).
async fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Help { command, all } => {
            // A named command always resolves against the *full* tree, with
            // or without `--all`: `meshnode help workflow` must answer even
            // though the greeting does not list it (D-L1.3).
            let mut cmd = if all || !command.is_empty() {
                full_command()
            } else {
                cli_command()
            };
            cmd.build();
            if all && command.is_empty() {
                print_grouped_help(&cmd)?;
                return Ok(0);
            }
            for part in &command {
                let Some(next) = cmd.find_subcommand(part).cloned() else {
                    bail!(
                        "no such command: `meshnode {}` — `meshnode help --all` lists every one",
                        command.join(" ")
                    );
                };
                cmd = next;
            }
            cmd.print_long_help()?;
            Ok(0)
        }
        Command::Up {
            listen,
            mesh_name,
            node_name,
            tags,
            advertise,
            supervised,
            new_mesh,
            detach,
        } => {
            // Before the config is even read: a demo's node must not
            // outlive the demo, and this is the only thing that survives
            // the demo being `SIGKILL`ed (`demo::arm_parent_death_signal`).
            if supervised {
                demo::arm_parent_death_signal();
            }
            let overrides = Overrides {
                listen,
                mesh_name,
                node_name,
                tags: (!tags.is_empty()).then_some(tags),
                advertise: (!advertise.is_empty()).then_some(advertise),
                ..Default::default()
            };
            // A configuration that does not load or check, and a data
            // directory from a newer release, exit with their own statuses:
            // the unit does not restart those, because a restart cannot
            // fix them (ENG-1).
            let refuse = |code: i32, e: &dyn std::fmt::Display| {
                eprintln!("{}", term::error_line(&format!("{e:#}")));
                Ok(code)
            };
            let mut config = match resolve_config(&cli.global, overrides.clone()) {
                Ok(config) => config,
                Err(e) => return refuse(mesh_core::daemon::exit_status::CONFIG, &e),
            };
            // Before the first-run question, because it changes the answer.
            // A machine whose mesh Director used to run inside itself is in
            // that mesh, and this used to greet it with "founding a new
            // mesh" and then do exactly that. `Node::start` adopts too; doing
            // it here first is what lets the banner tell the truth, and what
            // lets the config the node runs with include the settings the
            // adoption just wrote.
            let adopted = match config.layout().adopt_director_node() {
                Ok(moved) => moved,
                Err(e) => return refuse(mesh_core::daemon::exit_status::CONFIG, &e),
            };
            if !adopted.is_empty() {
                config = match resolve_config(&cli.global, overrides.clone()) {
                    Ok(config) => config,
                    Err(e) => return refuse(mesh_core::daemon::exit_status::CONFIG, &e),
                };
            }
            // Asked *before* the daemon writes one: the whole point is to
            // know whether this machine is about to found a mesh or rejoin
            // one it already belongs to (C3 §7.3 P3).
            let first_run = !config.layout().genesis_file().exists();
            // A machine reset out of its mesh waits to be adopted (the node
            // refuses on its own too); founding one here is `--new-mesh`.
            if new_mesh {
                if mesh_core::reset::clear(&config.layout())? {
                    eprintln!("This machine was reset out of its last mesh; founding a new one.");
                }
            } else if first_run && let Some(marker) = mesh_core::reset::marker(&config.layout())? {
                return refuse(mesh_core::daemon::exit_status::CONFIG, &marker.awaiting());
            }
            if detach {
                if first_run {
                    first_run_banner(&config);
                } else if !adopted.is_empty() {
                    adopted_banner(&config, &adopted);
                }
                return up_detached(&config).await;
            }
            let log_control = logging::init(&config);
            match config.check().context("configuration check failed") {
                // Said now, with a logger to say it to: the config was
                // resolved before this point, so a setting that no longer
                // does anything could not be reported any earlier.
                Ok(notes) => {
                    for note in notes {
                        tracing::warn!("{note}");
                    }
                }
                Err(e) => {
                    upgrade_guard_abandon(&cli.global, &e);
                    return refuse(mesh_core::daemon::exit_status::CONFIG, &e);
                }
            }

            // SIGHUP re-resolves the same file/env/flags and hot-applies
            // what can safely change (MASTERPLAN §12).
            let global = cli.global.clone();
            let hooks = mesh_core::daemon::RunHooks {
                reload: Some(Box::new(move || {
                    let merged = Overrides {
                        data_dir: global.data_dir.clone().or(overrides.data_dir.clone()),
                        ..overrides.clone()
                    };
                    NodeConfig::load(global.config.as_deref(), &merged)
                })),
                set_log_level: Some(Box::new(move |level| log_control.set_level(level))),
            };
            if first_run {
                first_run_banner(&config);
            } else if !adopted.is_empty() {
                adopted_banner(&config, &adopted);
            }
            match mesh_core::daemon::run(config, hooks).await {
                Ok(()) => Ok(0),
                Err(e @ mesh_core::CoreError::TooNew(_)) => {
                    refuse(mesh_core::daemon::exit_status::DATA_TOO_NEW, &e)
                }
                // Left a reset mesh, or started in a home that did: waiting
                // to be adopted, which a restart would not change.
                Err(e @ mesh_core::CoreError::AwaitingAdoption(_)) => {
                    refuse(mesh_core::daemon::exit_status::CONFIG, &e)
                }
                Err(e) => Err(e.into()),
            }
        }
        Command::Join {
            token,
            pair,
            wait,
            listen,
            via,
            tags,
            qr,
            json,
        } => {
            let overrides = Overrides {
                listen,
                tags: (!tags.is_empty()).then_some(tags),
                ..Default::default()
            };
            let config = adopt_before_asking(
                &cli.global,
                &overrides,
                resolve_config(&cli.global, overrides.clone())?,
            )?;
            let report = match (token, pair) {
                (Some(token), false) => mesh_core::membership::join_mesh(&config, &token, &via)
                    .await
                    .context("join failed")?,
                (None, true) => {
                    let wait = humantime::parse_duration(&wait)
                        .context("invalid --wait (try 10m, 30s)")?;
                    if json {
                        pair_join_scripted(&config, wait).await?
                    } else {
                        pair_join_interactive(&config, wait, qr).await?
                    }
                }
                (Some(_), true) => bail!("pass either an invite token or --pair, not both"),
                (None, false) => {
                    bail!("pass an invite token, or --pair to wait for LAN approval")
                }
            };
            if json {
                // The `joined` frame already said all of this, on the
                // channel the caller is parsing. A second, prose copy on
                // the same pipe is a parse error waiting to happen.
                return Ok(0);
            }
            println!(
                "joined mesh '{}' (accepted by {}) — {} member(s)",
                report.mesh_name,
                report.accepted_by.short(),
                report.members
            );
            println!(
                "{}",
                term::out().prose("run `meshnode up` to start participating.")
            );
            Ok(0)
        }
        Command::Demo { nodes, stop } => {
            if stop {
                demo::stop().await
            } else {
                demo::run(demo::Options { nodes }).await
            }
        }
        Command::Pair {
            join,
            approve,
            node,
            addr,
            browse,
            wait,
            listen,
            tags,
            qr,
        } => {
            // Which side of the handshake this machine is on is a *fact
            // about the machine*, not a decision to make: a node already in
            // a mesh lets somebody in; a node with no mesh asks to be let
            // in. The two flags exist for when the fact is wrong — an old
            // genesis on a machine being re-paired, say — and for scripts
            // that would rather say than infer.
            let overrides = Overrides {
                listen,
                tags: (!tags.is_empty()).then_some(tags.clone()),
                ..Default::default()
            };
            let config = adopt_before_asking(
                &cli.global,
                &overrides,
                resolve_config(&cli.global, overrides.clone())?,
            )?;
            let in_a_mesh = config.layout().genesis_file().exists();
            let approving = approve || (!join && in_a_mesh);
            if approving {
                let browse =
                    humantime::parse_duration(&browse).context("invalid --browse (try 3s, 30s)")?;
                let ctx = Ctx::new(&cli.global)?;
                invite_approve(&ctx, node, addr, browse, tags, qr).await
            } else {
                // Refused rather than ignored: a flag that silently does
                // nothing is how somebody spends ten minutes wondering why
                // the LAN scan never found them.
                if addr.is_some() {
                    bail!(
                        "--addr belongs to the machine doing the approving. This one is waiting \
                         to be let in — run `meshnode pair --addr HOST:PORT` on a mesh member \
                         instead, naming this machine's address"
                    );
                }
                if node.is_some() {
                    bail!(
                        "--node picks which waiting machine to admit; this one is the machine waiting"
                    );
                }
                let wait =
                    humantime::parse_duration(&wait).context("invalid --wait (try 10m, 30s)")?;
                let report = pair_join_interactive(&config, wait, qr).await?;
                println!(
                    "joined mesh '{}' (accepted by {}) — {} member(s)",
                    report.mesh_name,
                    report.accepted_by.short(),
                    report.members
                );
                println!(
                    "{}",
                    term::out().prose("run `meshnode up` to start participating.")
                );
                Ok(0)
            }
        }
        Command::Cc {
            on,
            verbose,
            local_only,
            timeout,
            argv,
        } => {
            // `Ctx::new` is what needs a user identity, and a build that
            // ran `meshnode cc` without one should compile rather than
            // stop — the fallback rule starts here, before the mesh is
            // even reached.
            let ctx = match Ctx::new(&cli.global) {
                Ok(ctx) => ctx,
                Err(e) => {
                    if verbose {
                        eprintln!("meshnode cc: local fallback ({e})");
                    }
                    let Some((compiler, args)) = argv.split_first() else {
                        bail!("`meshnode cc` needs a compiler");
                    };
                    return meshware_node::farm::run_local(compiler, args).await;
                }
            };
            meshware_node::farm::run(
                &ctx,
                meshware_node::farm::Options {
                    argv,
                    on,
                    verbose,
                    local_only,
                    timeout_secs: timeout,
                },
            )
            .await
        }
        Command::Adopt {
            targets,
            parallel,
            binary,
            install_path,
            sudo,
            no_sudo,
            no_fleet_upgrade,
            no_firewall,
            tags,
            ssh_options,
            via,
            dry_run,
            force,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let opts = adopt::Options {
                targets,
                parallel,
                binary,
                install_path,
                sudo,
                no_sudo,
                fleet_upgrade: !no_fleet_upgrade,
                open_firewall: !no_firewall,
                tags,
                ssh_options,
                via,
                dry_run,
                force,
            };
            let outcomes = adopt::run(&ctx, opts).await?;
            let failed = format::print_adopted(&outcomes, output == Output::Json)?;
            // Non-zero when any host failed. A rack adopt that quietly
            // succeeds for nine of ten is a rack adopt somebody will
            // discover next week.
            Ok(i32::from(failed > 0))
        }
        Command::Status { totals, output } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Status).await?;
            let IpcResponse::Status(s) = resp else {
                return unexpected(resp);
            };
            // Two reads rather than one enlarged answer: `status` is what
            // *this daemon* is doing and is on the `/healthz` path, and the
            // strip is a fold over every member. A flag nobody passed must
            // not make the liveness probe sum the mesh.
            let strip = if totals {
                let resp = ctx.ipc(IpcRequest::Totals).await?;
                let IpcResponse::Totals(t) = resp else {
                    return unexpected(resp);
                };
                Some(*t)
            } else {
                None
            };
            format::print_status(&s, strip.as_ref(), output == Output::Json)?;
            Ok(0)
        }
        Command::Whoami { output } => {
            let ctx = Ctx::new(&cli.global)?;
            let key_path = mesh_core::identity::cli_user_identity_path(
                &ctx.config.data_dir,
                cli.global.user_identity.as_deref(),
            );
            // **The one command that has to work when nothing else does.**
            // Every refusal on this machine ends by telling the person to
            // run `whoami` and send their key to an admin — so a `whoami`
            // that itself fails because the node will not talk to them is
            // a closed loop with no way out of it. The node's answer is
            // richer (roles, grants, whether the request is pending), but
            // the key and where it lives are ours to print either way.
            match ctx.ipc(IpcRequest::WhoAmI).await {
                Ok(IpcResponse::WhoAmI(info)) => {
                    format::print_whoami(&info, &key_path, output == Output::Json)?;
                    Ok(0)
                }
                Ok(other) => unexpected(other),
                Err(e) => {
                    format::print_whoami_offline(
                        &ctx.user_id(),
                        &key_path,
                        &e.to_string(),
                        output == Output::Json,
                    )?;
                    Ok(1)
                }
            }
        }
        Command::Definitions { output } => {
            let ctx = Ctx::new(&cli.global)?;
            let IpcResponse::Definitions(rows) = ctx.ipc(IpcRequest::Definitions).await? else {
                bail!("unexpected response to definitions");
            };
            // The catch-up half of the answer is the node view's sync
            // reading, not a second computation: a node's op-log position
            // *is* its definition version.
            let IpcResponse::Nodes(nodes) = ctx.ipc(IpcRequest::Nodes).await? else {
                bail!("unexpected response to nodes");
            };
            format::print_definitions(&rows, &nodes, output == Output::Json)?;
            Ok(0)
        }
        Command::Cache { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(CacheCmd::Stats {
                output: Output::Table,
            }) {
                CacheCmd::Stats { output } => {
                    let resp = ctx.ipc(IpcRequest::CacheStats).await?;
                    let IpcResponse::CacheStats(stats) = resp else {
                        return unexpected(resp);
                    };
                    format::print_cache_stats(&stats, output == Output::Json)?;
                    Ok(0)
                }
                CacheCmd::Purge { target, yes } => {
                    // A purge is recoverable but not free: every entry it
                    // drops is a job somebody will run again. `--yes` is the
                    // house rule for destructive verbs (§14.2) and this is
                    // one of them.
                    if !yes {
                        bail!(
                            "`cache purge` makes the mesh redo work it has already done — \
                             pass --yes"
                        );
                    }
                    let scope = match target.as_deref() {
                        None => mesh_proto::PurgeScope::All,
                        Some(t) if t.starts_with("act-") => mesh_proto::PurgeScope::Key(
                            mesh_proto::ActionKey::parse(t)
                                .map_err(|_| anyhow::anyhow!("not an action key: {t}"))?,
                        ),
                        Some(t) => mesh_proto::PurgeScope::Service(t.to_string()),
                    };
                    let resp = ctx.ipc(IpcRequest::CachePurge { scope }).await?;
                    match resp {
                        IpcResponse::Notice { message } => {
                            println!("{message}");
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
            }
        }
        Command::Plan {
            on,
            service,
            cpus,
            mem,
            disk,
            gpus,
            vram_mb,
            device,
            fanout,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let mut env = parse_requirements(cpus, mem.as_deref(), disk.as_deref())?.to_env();
            env.extend(parse_device_request(
                gpus,
                vram_mb,
                device.as_deref(),
                false,
            )?);
            let fanout = parse_fanout(fanout.as_deref())?;
            let resp = ctx
                .ipc(IpcRequest::Plan {
                    on,
                    service,
                    env,
                    fanout,
                })
                .await?;
            let IpcResponse::Plan(estimate) = resp else {
                return unexpected(resp);
            };
            let refused = estimate.refusal.is_some();
            format::print_plan(&estimate, output == Output::Json)?;
            // A plan for work that would not start exits non-zero, so a
            // script can gate on it without parsing prose.
            Ok(i32::from(refused))
        }
        Command::Gangs { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(GangCmd::Ls {
                all: false,
                output: Output::Table,
            }) {
                GangCmd::Ls { all, output } => {
                    let resp = ctx.ipc(IpcRequest::Gangs { all }).await?;
                    let IpcResponse::Gangs(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_gangs(&rows, output == Output::Json)?;
                    Ok(0)
                }
                GangCmd::Show { gang, output } => {
                    let gang = mesh_proto::GangId::parse(&gang)
                        .map_err(|_| anyhow::anyhow!("not a gang id: {gang}"))?;
                    let resp = ctx.ipc(IpcRequest::GangShow { gang }).await?;
                    let IpcResponse::GangShow(view) = resp else {
                        return unexpected(resp);
                    };
                    format::print_gang(&view, output == Output::Json)?;
                    Ok(0)
                }
            }
        }
        Command::Queue {
            by_principal,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Queue).await?;
            let IpcResponse::Queue(rows) = resp else {
                return unexpected(resp);
            };
            format::print_queue(&rows, by_principal, output == Output::Json)?;
            Ok(0)
        }
        Command::Sinks { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(SinksCmd::Ls { output }) {
                SinksCmd::Ls { output } => {
                    let resp = ctx.ipc(IpcRequest::Sinks).await?;
                    let IpcResponse::Sinks(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_sinks(&rows, output == Output::Json)?;
                    Ok(0)
                }
                SinksCmd::Ddl { name, dialect } => {
                    let rows = match ctx.ipc(IpcRequest::Sinks).await? {
                        IpcResponse::Sinks(rows) => rows,
                        other => return unexpected(other),
                    };
                    let row = rows
                        .iter()
                        .find(|r| r.name == name)
                        .ok_or_else(|| anyhow!("no sink named {name}"))?;
                    let kind = mesh_proto::SinkKind::parse(&row.kind)
                        .ok_or_else(|| anyhow!("sink {name} has an unknown kind"))?;
                    let dialect = match dialect.as_deref() {
                        Some(d) => mesh_core::sinks::DdlDialect::parse(d).ok_or_else(|| {
                            anyhow!(
                                "'{d}' is not a dialect — try {}",
                                mesh_core::sinks::DdlDialect::ALL
                                    .iter()
                                    .map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })?,
                        None => mesh_core::sinks::DdlDialect::for_kind(kind).ok_or_else(|| {
                            anyhow!(
                                "a {} sink has no schema to state — pass --dialect if you want \
                                 one anyway",
                                kind.as_str()
                            )
                        })?,
                    };
                    // Reconstructed from the row rather than fetched: `ddl`
                    // needs only the name, tables and database, and none of
                    // those is a secret.
                    let mut sink = blank_sink(&name, kind);
                    sink.database = row.database.clone();
                    sink.tables =
                        mesh_proto::SinkTables::parse(&row.tables.join(",")).unwrap_or_default();
                    print!("{}", mesh_core::sinks::ddl(&sink, dialect));
                    Ok(0)
                }
                SinksCmd::Test { name } => {
                    // The CLI tests a sink that exists; Director tests the
                    // form before it is saved. Both send a record.
                    let rows = match ctx.ipc(IpcRequest::Sinks).await? {
                        IpcResponse::Sinks(rows) => rows,
                        other => return unexpected(other),
                    };
                    let row = rows
                        .iter()
                        .find(|r| r.name == name)
                        .ok_or_else(|| anyhow!("no live sink named {name}"))?;
                    let kind = mesh_proto::SinkKind::parse(&row.kind)
                        .ok_or_else(|| anyhow!("sink {name} has an unknown kind"))?;
                    let mut sink = blank_sink(&name, kind);
                    // `target` is what an operator *reads*, and for a file
                    // sink it is a resolved path or a phrase — neither of
                    // which is a URL. Copying it into `url` regardless put
                    // a filesystem path in the field the egress allowlist
                    // parses a host out of.
                    if kind.needs_url() {
                        sink.url = row.target.clone();
                    }
                    sink.database = row.database.clone();
                    // Without this the probe would assemble a different
                    // pool than the sink actually has and report on a sink
                    // nobody asked about.
                    sink.run_on = row.run_on.clone();
                    sink.tables =
                        mesh_proto::SinkTables::parse(&row.tables.join(",")).unwrap_or(sink.tables);
                    // The credential reference is not on the row (it is not
                    // a secret, but it is not something a listing needs
                    // either), so a CLI test exercises reachability and the
                    // allowlist rather than the token. Director's test,
                    // which holds the form, covers the token too.
                    let resp = ctx
                        .ipc(IpcRequest::SinkTest {
                            sink: Box::new(sink),
                        })
                        .await?;
                    match resp {
                        IpcResponse::SinkTest { ok, status } => {
                            // Verbatim, whichever way it went: the response
                            // is the whole point of the button.
                            println!("{}  {status}", if ok { "ok  " } else { "FAIL" });
                            Ok(i32::from(!ok))
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
                SinksCmd::Add {
                    name,
                    kind,
                    url,
                    plugin,
                    database,
                    file,
                    credential,
                    read_credential,
                    tables,
                    run_on,
                    batch,
                    spool_mb,
                    payloads,
                    payload_max_mb,
                    hold_for_export_days,
                    disabled,
                    description,
                    dry_run,
                } => {
                    let Some(kind) = mesh_proto::SinkKind::parse(&kind) else {
                        // The list, with what each one gets you — the same
                        // sentence Director shows at selection, because a
                        // wrong guess here is exactly the moment somebody
                        // needs it.
                        eprintln!("'{kind}' is not a destination.\n");
                        format::print_sink_kinds();
                        return Ok(2);
                    };
                    // Both used to be accepted and quietly misread: a
                    // `--plugin` on a warehouse became its database name.
                    if kind == mesh_proto::SinkKind::Plugin && database.is_some() {
                        bail!(
                            "--database names a database or an index prefix, which a plugin does \
                             not have — its program is --plugin"
                        );
                    }
                    if kind != mesh_proto::SinkKind::Plugin && plugin.is_some() {
                        bail!(
                            "--plugin is for --kind plugin; a {} sink is reached at --url",
                            kind.as_str()
                        );
                    }
                    let tables =
                        mesh_proto::SinkTables::parse(&tables).map_err(|e| anyhow!("{e}"))?;
                    let mut records = mesh_proto::SinkRecords::default();
                    if let Some(b) = batch {
                        records.batch = b;
                    }
                    let mut on_failure = mesh_proto::SinkOnFailure::default();
                    if let Some(mb) = spool_mb {
                        on_failure.spool_bytes = mb.saturating_mul(1024 * 1024);
                    }
                    if let Some(days) = hold_for_export_days {
                        on_failure.hold_for_export_days = days;
                    }
                    // §19 Q2, decided: a **per-kind** default. A log store
                    // is what raw stdout is for; a warehouse and an unknown
                    // webhook are not, and one global default would have
                    // been either too timid for VictoriaLogs or too
                    // reckless for everything else.
                    let payloads = mesh_proto::SinkPayloads {
                        enabled: payloads || kind.payloads_by_default(),
                        max_bytes: payload_max_mb
                            .map(|mb| mb.saturating_mul(1024 * 1024))
                            // 64 MB. Large enough for any log worth reading,
                            // small enough that a core dump does not reach a
                            // per-gigabyte destination by accident.
                            .unwrap_or(64 * 1024 * 1024),
                    };
                    let sink = mesh_proto::Sink {
                        name: name.clone(),
                        kind,
                        url: url.unwrap_or_default(),
                        // One field, three meanings, by kind: the program
                        // for a plugin, the file for ndjson, the database
                        // or index prefix for the rest. Adding a field
                        // would change `Sink`'s encoding and invalidate
                        // every sink record ever signed. Director always
                        // had the third; the CLI had no way to say it.
                        database: match kind {
                            mesh_proto::SinkKind::Ndjson => file.unwrap_or_default(),
                            mesh_proto::SinkKind::Plugin => plugin.unwrap_or_default(),
                            _ => database.unwrap_or_default(),
                        },
                        credential: credential.unwrap_or_default(),
                        read_credential: read_credential.unwrap_or_default(),
                        tables,
                        records,
                        payloads,
                        on_failure,
                        run_on: run_on.unwrap_or_default(),
                        enabled: !disabled,
                        description: description.unwrap_or_default(),
                        created_by: ctx.user_id(),
                        created_at_micros: mesh_proto::now_micros(),
                        signature: Vec::new(),
                    };
                    if let Some(why) = sink.refusal() {
                        return Err(anyhow!("{why}"));
                    }
                    if dry_run {
                        format::print_sink_definition(&sink);
                        println!();
                        println!("valid — nothing published (drop --dry-run to publish)");
                        return Ok(0);
                    }
                    let signed = mesh_core::sinks::sign(&ctx.user, sink);
                    let resp = ctx
                        .ipc(IpcRequest::SinkPut {
                            sink: Box::new(signed),
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("sink {name} saved.");
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
                SinksCmd::Rm { name } => {
                    let signature = mesh_core::sinks::sign_remove(&ctx.user, &name);
                    let resp = ctx
                        .ipc(IpcRequest::SinkRemove {
                            name: name.clone(),
                            signature,
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("sink {name} retired. Rows already exported are untouched.");
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
                SinksCmd::Resume => {
                    let resp = ctx.ipc(IpcRequest::SinksResume).await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("sinks resumed. Exporting continues from each sink's cursor.");
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
            }
        }
        Command::Retention { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(RetentionCmd::Show { output }) {
                RetentionCmd::Show { output } => {
                    let resp = ctx.ipc(IpcRequest::RetentionShow).await?;
                    let IpcResponse::Retention(view) = resp else {
                        return unexpected(resp);
                    };
                    format::print_retention(&view, output == Output::Json)?;
                    Ok(0)
                }
                RetentionCmd::Set {
                    windows,
                    floor,
                    dry_run,
                    output,
                } => {
                    // Edited on top of what the mesh currently has, never
                    // built from defaults: `retention set runs=30d` says
                    // nothing about `audit`, and a tool that read silence
                    // as "restore the default" would quietly shorten the
                    // audit log while somebody was tuning disk.
                    let live = match ctx.ipc(IpcRequest::RetentionShow).await? {
                        IpcResponse::Retention(v) => v,
                        other => return unexpected(other),
                    };
                    let edited = mesh_core::retention::apply_edits(&live.policy, &windows, &floor)
                        .map_err(|e| anyhow!("{e}"))?;
                    if let Some(why) = edited.refusal() {
                        return Err(anyhow!("{why}"));
                    }
                    if dry_run {
                        format::print_retention_preview(&live, &edited, output == Output::Json)?;
                        println!();
                        println!("nothing signed (drop --dry-run to publish)");
                        return Ok(0);
                    }
                    let signed =
                        mesh_core::retention::sign(&ctx.user, edited, mesh_proto::now_micros());
                    let resp = ctx
                        .ipc(IpcRequest::RetentionPut {
                            policy: Box::new(signed),
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            let after = match ctx.ipc(IpcRequest::RetentionShow).await? {
                                IpcResponse::Retention(v) => v,
                                other => return unexpected(other),
                            };
                            format::print_retention(&after, output == Output::Json)?;
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
                RetentionCmd::Reset => {
                    let at = mesh_proto::now_micros();
                    let signature = mesh_core::retention::sign_reset(&ctx.user, at);
                    let resp = ctx
                        .ipc(IpcRequest::RetentionReset {
                            at_micros: at,
                            signature,
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("retention is back to the shipped defaults.");
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
            }
        }
        Command::State {
            cmd: Some(StateCmd::Rebuild { from, yes }),
            ..
        } => {
            let config = resolve_config(&cli.global, Overrides::default())?;
            if !yes {
                bail!(
                    "rebuild moves the op-log in {} aside and fetches this node's history from \
                     {} — re-run with --yes",
                    config.layout().state_dir().display(),
                    from.join(", ")
                );
            }
            if mesh_core::ipc::request(
                &config.socket_path(),
                &Keypair::generate()?,
                IpcRequest::Status,
            )
            .await
            .is_ok()
            {
                bail!("the daemon is running — stop it first (`systemctl stop meshnode`)");
            }
            let report = mesh_core::membership::rebuild_oplog(&config, &from)
                .await
                .context("rebuild failed; the op-log is as it was")?;
            println!(
                "rebuilt the op-log from {}: {} ops",
                report.from.short(),
                report.ops
            );
            if let Some(kept) = report.kept {
                println!(
                    "the op-log that would not open is kept at {}",
                    kept.display()
                );
            }
            println!("\n{}", mesh_core::backup::RESTORE_HOLD_NOTE);
            println!(
                "{}",
                term::out().prose("run `meshnode up` to start the node.")
            );
            Ok(0)
        }
        Command::State { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            let cmd = cmd.unwrap_or(StateCmd::Show { output });
            // The census answers a different question with a different
            // response type, so it leaves the shared dispatch below rather
            // than pretending to be a `StateStats`.
            if let StateCmd::Pressure { output } = cmd {
                let resp = ctx.ipc(IpcRequest::Backpressure).await?;
                let IpcResponse::Backpressure(view) = resp else {
                    return unexpected(resp);
                };
                format::print_backpressure(&view, output == Output::Json)?;
                // Non-zero when something is being refused, so a cron job
                // watching for the bad day does not have to parse a table.
                return Ok(i32::from(!view.entities.is_empty()));
            }
            if let StateCmd::Census { output } = cmd {
                let resp = ctx.ipc(IpcRequest::OpCensus).await?;
                let IpcResponse::OpCensus(census) = resp else {
                    return unexpected(resp);
                };
                format::print_op_census(&census, output == Output::Json)?;
                return Ok(0);
            }
            let (req, out) = match cmd {
                StateCmd::Census { .. } | StateCmd::Pressure { .. } | StateCmd::Rebuild { .. } => {
                    unreachable!("handled above")
                }
                StateCmd::Show { output } => (IpcRequest::StateShow, output),
                StateCmd::Compact {
                    horizon,
                    dry_run,
                    output,
                } => {
                    let horizon_secs = match horizon.as_deref() {
                        None => None,
                        Some(text) => Some(
                            humantime::parse_duration(text)
                                .map_err(|_| {
                                    anyhow!("--horizon: '{text}' is not a duration (try 7d, 72h)")
                                })?
                                .as_secs(),
                        ),
                    };
                    (
                        IpcRequest::StateCompact {
                            horizon_secs,
                            dry_run,
                        },
                        output,
                    )
                }
            };
            let resp = ctx.ipc(req).await?;
            let IpcResponse::StateStats(stats) = resp else {
                return unexpected(resp);
            };
            format::print_state(&stats, out == Output::Json)?;
            Ok(0)
        }
        Command::Enrol {
            as_,
            no_wait,
            timeout,
        } => run_enrol(&cli.global, as_, no_wait, timeout).await,
        Command::Identity {
            cmd: IdentityCmd::Show { output },
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::IdentityShow).await?;
            let IpcResponse::Identity(i) = resp else {
                return unexpected(resp);
            };
            format::print_identity(&i, output == Output::Json)?;
            if output == Output::Table {
                println!("cli user  {}", ctx.user_id());
            }
            Ok(0)
        }
        // These four never build a `Ctx` and never reach the daemon. A `Ctx`
        // loads the user key, which is precisely what fails when the key is
        // locked — so routing `identity unlock` through one would make it
        // unrunnable by the only person who needs it.
        Command::Identity {
            cmd: IdentityCmd::Hardware { cmd },
        } => run_identity_hardware(&cli.global, cmd).await,
        Command::Identity {
            cmd: IdentityCmd::Protect,
        } => {
            let path = identity_key_path(&cli.global)?;
            identity_protect::protect(&path)
        }
        Command::Identity {
            cmd: IdentityCmd::Unprotect { yes },
        } => {
            let path = identity_key_path(&cli.global)?;
            identity_protect::unprotect(&path, yes)
        }
        Command::Identity {
            cmd: IdentityCmd::Unlock,
        } => {
            let path = identity_key_path(&cli.global)?;
            identity_protect::unlock(&path)
        }
        Command::Identity {
            cmd: IdentityCmd::Lock,
        } => {
            let path = identity_key_path(&cli.global)?;
            identity_protect::lock(&path)
        }
        Command::Identity {
            cmd: IdentityCmd::Status { output },
        } => {
            let path = identity_key_path(&cli.global)?;
            identity_protect::status(&path, output == Output::Json)
        }
        Command::Invite {
            cmd:
                InviteCmd::Approve {
                    node,
                    addr,
                    browse,
                    tags,
                    qr,
                },
        } => {
            let browse = humantime::parse_duration(&browse).context("invalid --browse")?;
            let ctx = Ctx::new(&cli.global)?;
            invite_approve(&ctx, node, addr, browse, tags, qr).await
        }
        Command::Invite {
            cmd: InviteCmd::Ls { all, output },
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Invites).await?;
            let IpcResponse::Invites(rows) = resp else {
                return unexpected(resp);
            };
            let now = mesh_proto::now_micros();
            let rows: Vec<_> = rows
                .into_iter()
                .filter(|r| all || (!r.revoked && r.used < r.uses && r.expires_at_micros > now))
                .collect();
            format::print_invites(&rows, all, output == Output::Json)?;
            Ok(0)
        }
        Command::Invite {
            cmd: InviteCmd::Revoke { invite },
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Invites).await?;
            let IpcResponse::Invites(rows) = resp else {
                return unexpected(resp);
            };
            let want = invite.trim();
            let matches: Vec<_> = rows
                .iter()
                .filter(|r| r.invite_id.to_display_string().starts_with(want))
                .collect();
            let row = match matches.as_slice() {
                [] => bail!("no invite matches {want:?} — `meshnode invite ls --all` lists them"),
                [row] => *row,
                _ => bail!(
                    "{want:?} matches {} invites — give more of the id",
                    matches.len()
                ),
            };
            if row.revoked {
                println!("invite {} was already revoked", row.invite_id);
                return Ok(0);
            }
            let by = ctx.user_id();
            let signature = ctx
                .user
                .sign(&mesh_proto::invite_revoke_signing_bytes(
                    &row.invite_id,
                    &by,
                ))
                .to_vec();
            let resp = ctx
                .ipc(IpcRequest::InviteRevoke {
                    invite_id: row.invite_id,
                    by,
                    signature,
                })
                .await?;
            let IpcResponse::Ok = resp else {
                return unexpected(resp);
            };
            println!(
                "revoked invite {} ({} of {} use(s) spent). Nodes that already joined with it \
                 are still members.",
                row.invite_id, row.used, row.uses
            );
            Ok(0)
        }
        Command::Invite {
            cmd:
                InviteCmd::Create {
                    ttl,
                    uses,
                    tags,
                    addrs: manual,
                },
        } => {
            let ttl_secs = humantime::parse_duration(&ttl)
                .context("invalid --ttl (try 24h, 30m, 7d)")?
                .as_secs();
            let ctx = Ctx::new(&cli.global)?;
            // Fetch bootstrap info, then build + sign the token *locally* —
            // only this machine holds the admin's user key (MASTERPLAN §7.1).
            let resp = ctx.ipc(IpcRequest::InviteBootstrap).await?;
            let IpcResponse::InviteBootstrap {
                mesh_id,
                node_id,
                addrs,
            } = resp
            else {
                return unexpected(resp);
            };
            // `--addr` entries go first: the operator knows a reachable
            // address the mesh may not (MASTERPLAN M7 remote invite UX).
            let mut bootstrap: Vec<String> = Vec::new();
            for a in manual.iter().chain(addrs.iter()) {
                if mesh_proto::parse_relay_candidate(a).is_some() {
                    continue; // joiners are not members; relays are useless to them
                }
                if !bootstrap.contains(a) {
                    bootstrap.push(a.clone());
                }
            }
            let params = membership::InviteParams {
                ttl_secs,
                uses,
                tags,
                bootstrap,
            };
            let (token, public) = membership::build_invite(mesh_id, &ctx.user, &params, node_id)?;
            let resp = ctx.ipc(IpcRequest::InvitePublish { public }).await?;
            let IpcResponse::Ok = resp else {
                return unexpected(resp);
            };
            if !token.bootstrap.iter().any(|a| is_public_bootstrap(a)) {
                eprintln!(
                    "{}",
                    term::err().prose(&format!(
                        "warning: this token carries only LAN/loopback addresses ({}) — a joiner \
                     outside this network cannot reach them. Add a public endpoint with \
                     `--addr HOST:PORT`, run `meshnode up --advertise HOST:PORT` on a public \
                     node, or pass `meshnode join --via HOST:PORT` on the joiner.",
                        token.bootstrap.join(", ")
                    ))
                );
            }
            eprintln!(
                "invite {} ({} use(s)). On the new node, run:\n",
                token.invite_id, token.uses
            );
            println!("meshnode join {}", token.encode());
            Ok(0)
        }
        Command::Nodes {
            cmd,
            devices,
            node,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if devices {
                if cmd.is_some() {
                    bail!("--devices lists accelerators; it takes no subcommand");
                }
                let resp = ctx.ipc(IpcRequest::Devices { node }).await?;
                let IpcResponse::Devices(rows) = resp else {
                    return unexpected(resp);
                };
                format::print_devices(&rows, output == Output::Json)?;
                return Ok(0);
            }
            if node.is_some() {
                bail!("--node selects which node's devices to list; it needs --devices");
            }
            match cmd.unwrap_or(NodesCmd::Ls) {
                NodesCmd::Ls => {
                    let resp = ctx.ipc(IpcRequest::Nodes).await?;
                    let IpcResponse::Nodes(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_nodes_with(&rows, output == Output::Json)?;
                    Ok(0)
                }
                NodesCmd::Contribute {
                    node,
                    on,
                    off,
                    reason,
                } => {
                    if on == off {
                        bail!(
                            "say which: --on lets the mesh place work on this node, --off stops it"
                        );
                    }
                    let node_id = resolve_node(&ctx, &node).await?;
                    let name = match ctx.ipc(IpcRequest::Nodes).await? {
                        IpcResponse::Nodes(rows) => rows
                            .iter()
                            .find(|r| r.node_id == node_id)
                            .map(|r| r.name.clone())
                            .unwrap_or_else(|| node_id.short()),
                        _ => node_id.short(),
                    };
                    let mut decision = mesh_proto::NodeExecution {
                        node_id,
                        allowed: on,
                        decided_by: ctx.user_id(),
                        decided_at_micros: mesh_proto::now_micros(),
                        reason: reason.trim().to_string(),
                        signature: Vec::new(),
                    };
                    decision.signature = ctx.sign(&decision.signing_bytes())?;
                    let resp = ctx
                        .ipc(IpcRequest::NodeContributeSet {
                            decision: Box::new(decision),
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            if on {
                                println!("{name} may run work for this mesh.");
                            } else {
                                println!(
                                    "{name} will not be given work. It stays a member — \
                                     state, submission and terminals are unaffected."
                                );
                            }
                            Ok(0)
                        }
                        IpcResponse::Denied { message } => Err(anyhow!("{message}")),
                        other => unexpected(other),
                    }
                }
                NodesCmd::Diff {
                    left,
                    right,
                    collector,
                    all,
                    output,
                } => {
                    nodes_diff(
                        &ctx,
                        &left,
                        &right,
                        collector.as_deref(),
                        all,
                        output == Output::Json,
                    )
                    .await
                }
                NodesCmd::Tag {
                    node,
                    add,
                    rm,
                    clear,
                } => {
                    if add.is_empty() && rm.is_empty() && !clear {
                        bail!("pass --add and/or --rm tags, or --clear");
                    }
                    let Some(node) = node else {
                        if clear {
                            bail!(
                                "--clear only applies to tags assigned to another node.\n\nThis node's own tags come from its config and its hardware; remove one with `--rm <tag>`."
                            );
                        }
                        ctx.ipc(IpcRequest::TagsUpdate { add, remove: rm }).await?;
                        println!("tags updated (gossip propagates them mesh-wide)");
                        return Ok(0);
                    };
                    let node_id = resolve_node(&ctx, &node).await?;
                    // Read-modify-write against the *assigned* half only.
                    // The op carries the whole list because that is what
                    // converges under one clock, so the delta the operator
                    // typed is applied here, against what the mesh
                    // currently says — never against the node's own
                    // announcements, which are not ours to rewrite.
                    let IpcResponse::AssignedTags { tags: current, .. } =
                        ctx.ipc(IpcRequest::AssignedTags { node_id }).await?
                    else {
                        bail!("could not read the tags assigned to {}", node_id.short());
                    };
                    let mut tags: Vec<String> = if clear {
                        Vec::new()
                    } else {
                        current
                            .iter()
                            .filter(|t| !rm.contains(t))
                            .cloned()
                            .collect()
                    };
                    for t in &add {
                        if !tags.contains(t) {
                            tags.push(t.clone());
                        }
                    }
                    // Say so rather than authoring an op that changes
                    // nothing: `--rm prod` on a node whose `prod` is its
                    // own announcement would otherwise look like it worked.
                    if !clear && tags == current {
                        let announced: Vec<String> = rm
                            .iter()
                            .filter(|t| !current.contains(t))
                            .cloned()
                            .collect();
                        if !announced.is_empty() {
                            bail!(
                                "{} is not assigned {} — if the node carries {} it is announcing it itself, and only that node can stop.\n\nRun there: meshnode nodes tag --rm {}",
                                node_id.short(),
                                announced.join(", "),
                                if announced.len() == 1 {
                                    "that tag"
                                } else {
                                    "those tags"
                                },
                                announced.join(" --rm "),
                            );
                        }
                        println!(
                            "no change: {} already has {}",
                            node_id.short(),
                            tags.join(", ")
                        );
                        return Ok(0);
                    }
                    let mut assignment = mesh_proto::NodeTags {
                        node_id,
                        tags: tags.clone(),
                        by: ctx.user_id(),
                        at_micros: mesh_proto::now_micros(),
                        signature: Vec::new(),
                    };
                    assignment.signature = ctx.sign(&assignment.signing_bytes())?;
                    ctx.ipc(IpcRequest::NodeTags { assignment }).await?;
                    if tags.is_empty() {
                        println!("{}: no tags assigned any more", node_id.short());
                    } else {
                        println!("{} assigned: {}", node_id.short(), tags.join(", "));
                    }
                    println!(concat!(
                        "Assigned in the mesh — it survives that node's reboots, ",
                        "and `--on tag=…` sees it as soon as gossip lands."
                    ));
                    Ok(0)
                }
                NodesCmd::Drain {
                    node,
                    deadline,
                    no_deadline,
                    keep_system,
                    lift,
                } => {
                    let node_id = resolve_node(&ctx, &node).await?;
                    // Read-modify-write on the assigned half, as `nodes tag`
                    // does: a drain is two assigned tags, replaced whole.
                    let IpcResponse::AssignedTags { tags: current, .. } =
                        ctx.ipc(IpcRequest::AssignedTags { node_id }).await?
                    else {
                        bail!("could not read the tags assigned to {}", node_id.short());
                    };
                    let mut tags: Vec<String> = current
                        .iter()
                        .filter(|t| !mesh_proto::is_drain_tag(t))
                        .cloned()
                        .collect();
                    let by = if lift {
                        None
                    } else if no_deadline {
                        Some(None)
                    } else {
                        let wait = humantime::parse_duration(&deadline).with_context(|| {
                            format!("--deadline {deadline:?} is not a duration")
                        })?;
                        Some(Some(mesh_proto::now_micros().saturating_add(
                            u64::try_from(wait.as_micros()).unwrap_or(u64::MAX),
                        )))
                    };
                    match by {
                        None if tags.len() == current.len() => {
                            println!("{} is not being drained", node_id.short());
                            return Ok(0);
                        }
                        None => {}
                        Some(at) => tags.extend(mesh_proto::drain_tags(at, keep_system)),
                    }
                    let mut assignment = mesh_proto::NodeTags {
                        node_id,
                        tags,
                        by: ctx.user_id(),
                        at_micros: mesh_proto::now_micros(),
                        signature: Vec::new(),
                    };
                    assignment.signature = ctx.sign(&assignment.signing_bytes())?;
                    ctx.ipc(IpcRequest::NodeTags { assignment }).await?;
                    match by {
                        None => println!(
                            "{}: drain lifted — it takes new work again",
                            node_id.short()
                        ),
                        Some(None) => println!(
                            "{}: draining — its instances move as their replacements prove \
                             themselves, with no deadline",
                            node_id.short()
                        ),
                        Some(Some(_)) => println!(
                            "{}: draining — its instances move as their replacements prove \
                             themselves; whatever is left in {deadline} is stopped{}",
                            node_id.short(),
                            if keep_system {
                                ", except system instances"
                            } else {
                                ""
                            }
                        ),
                    }
                    Ok(0)
                }
                NodesCmd::Addr { node, set } => {
                    let node_id = resolve_node(&ctx, &node).await?;
                    if set.is_empty() {
                        let IpcResponse::Nodes(rows) = ctx.ipc(IpcRequest::Nodes).await? else {
                            bail!("could not read the node list");
                        };
                        let Some(row) = rows.iter().find(|r| r.node_id == node_id) else {
                            bail!("no record for {}", node_id.short());
                        };
                        if output == Output::Json {
                            println!("{}", serde_json::to_string_pretty(&row.addrs)?);
                        } else if row.addrs.is_empty() {
                            println!("{}: no address recorded yet", row.name);
                        } else {
                            println!("{}:", row.name);
                            for a in &row.addrs {
                                println!("  {a}");
                            }
                        }
                        return Ok(0);
                    }
                    ctx.ipc(IpcRequest::NodeAddrRepair {
                        node_id,
                        addrs: set.clone(),
                    })
                    .await?;
                    println!("{} now recorded at {}", node_id.short(), set.join(", "));
                    println!(
                        "{}",
                        term::out().prose(
                            "gossip dials it within a second or two; \
                         `meshnode nodes ls` shows when it answers"
                        )
                    );
                    Ok(0)
                }
                NodesCmd::Quarantine { node, reason, lift } => {
                    let node_id = resolve_node(&ctx, &node).await?;
                    if !lift && reason.trim().is_empty() {
                        bail!(
                            "quarantine needs a --reason.\n\n\
                             A node nobody may use, with no reason attached, is a support \
                             ticket somebody opens at 3am — and the reason is what the next \
                             person reads before deciding whether to lift it."
                        );
                    }
                    let mut quarantine = mesh_proto::NodeQuarantine {
                        node_id,
                        quarantined: !lift,
                        reason: reason.clone(),
                        by: ctx.user_id(),
                        at_micros: mesh_proto::now_micros(),
                        signature: Vec::new(),
                    };
                    quarantine.signature = ctx.sign(&quarantine.signing_bytes())?;
                    ctx.ipc(IpcRequest::NodeQuarantine { quarantine }).await?;
                    if lift {
                        println!("node {} is no longer quarantined", node_id.short());
                        println!("It takes work again, and terminals are permitted.");
                    } else {
                        println!("node {} quarantined — {reason}", node_id.short());
                        println!();
                        println!("It will take no new work and nobody can open a terminal on it.");
                        println!(
                            "{}",
                            term::out().prose(
                                "Work already running is untouched: `meshnode drain` stops that."
                            )
                        );
                        println!("Lift with: meshnode nodes quarantine {node} --lift");
                    }
                    Ok(0)
                }
                NodesCmd::Revoke { node, reason, yes } => {
                    if !yes {
                        bail!("node revocation is permanent — re-run with --yes");
                    }
                    let node_id = resolve_node(&ctx, &node).await?;
                    let mut revocation = NodeRevocation {
                        node_id,
                        reason,
                        by: ctx.user_id(),
                        at_micros: mesh_proto::now_micros(),
                        signature: Vec::new(),
                    };
                    revocation.signature = ctx.sign(&revocation.signing_bytes())?;
                    ctx.ipc(IpcRequest::NodeRevoke { revocation }).await?;
                    println!("node {} revoked", node_id.short());
                    Ok(0)
                }
            }
        }
        Command::Ping { addr, output } => {
            let ctx = Ctx::new(&cli.global)?;
            // A name or id names a member, and a member is reached by id:
            // the daemon then picks the address (or the relay), instead of
            // the caller guessing at one. An `addr:port` is always a raw
            // probe — that is what it is for.
            let req = match node_target(&ctx, &addr).await {
                Some(node) => IpcRequest::PingNode { node },
                None => IpcRequest::PingPeer { addr },
            };
            let resp = ctx.ipc(req).await?;
            let IpcResponse::Ping(p) = resp else {
                return unexpected(resp);
            };
            format::print_ping(&p, output == Output::Json)?;
            // The peer is one object, and the next thing anybody wants
            // after "can I reach it" is to look at it (L2 P5). Printed
            // only when the peer turned out to be a member — a raw probe
            // of an address that is not in this mesh has no screen.
            if output != Output::Json && p.same_mesh {
                format::print_deep_link(
                    format::DeepLinkKind::Node,
                    &p.peer_node_id.to_display_string(),
                );
            }
            Ok(0)
        }
        Command::Run {
            command,
            on,
            port,
            register,
            volumes,
            preemptible,
            gang,
            gang_placement,
            gang_restart,
            env,
            timeout,
            fanout,
            priority,
            max_parallel,
            halt_on_fail,
            cpus,
            mem,
            disk,
            gpus,
            vram_mb,
            device,
            gpu_share,
            dry_run,
            json,
            outputs,
            expect,
            cacheable,
            cache_outputs,
            explain_key,
            race,
            singleton,
            fallback,
            notify,
            stream: stream_lines,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let fallback_local = match fallback.as_deref().map(str::trim) {
                None => false,
                Some("local") => true,
                Some(other) => bail!("--fallback takes `local` (got {other:?})"),
            };
            if fallback_local && (fanout.is_some() || gang.is_some() || race.is_some()) {
                bail!(
                    "--fallback local applies to a single run: a fan-out, a gang or a race is \
                     several machines, and this one is only ever one"
                );
            }
            let user_env = parse_env(&env)?;
            let mut env = user_env.clone();
            if let Some(name) = &singleton {
                let name = name.trim();
                if name.is_empty() {
                    bail!("--singleton needs a name");
                }
                env.push((mesh_proto::REQ_SINGLETON.to_string(), name.to_string()));
            }
            // `--cacheable` travels as reserved environment, like every
            // other flag on this command that the daemon reads and the
            // executor must not: `IpcRequest::Run` is re-decoded from a
            // signed envelope on the executing node, so growing it would
            // break a mixed-version mesh (§14.1).
            env.extend(cache_env(cacheable, cache_outputs.as_deref(), explain_key)?);
            env.extend(parse_requirements(cpus, mem.as_deref(), disk.as_deref())?.to_env());
            env.extend(parse_device_request(
                gpus,
                vram_mb,
                device.as_deref(),
                gpu_share,
            )?);
            env.extend(parse_port_request(&port)?);
            env.extend(parse_volume_request(&volumes)?);
            if let Some(name) = &register {
                if !mesh_core::registry::is_valid_service_name(name) {
                    bail!(
                        "--register {name:?} must be a DNS label: 1-63 letters, digits or \
                         '-', not starting or ending with '-', and not all digits — it \
                         becomes {name}.{}",
                        mesh_proto::DNS_SUFFIX
                    );
                }
                if port.is_empty() {
                    bail!(
                        "--register needs a --port: a registered name with no address \
                         resolves to nothing"
                    );
                }
                env.push((mesh_proto::REQ_SERVICE.to_string(), name.clone()));
            }
            if preemptible {
                env.push((mesh_proto::REQ_PREEMPTIBLE.to_string(), "1".into()));
            }
            // `--gang` reaches the daemon as reserved environment keys for
            // the same reason `--json` does: the executor
            // re-decodes `IpcRequest::Run` out of the signed envelope to
            // re-check authorization, so growing that request would break a
            // mixed-version mesh. The origin reads these off and replaces
            // them with the member's rank before anything is dispatched.
            env.extend(gang_env(
                gang,
                gang_placement.as_deref(),
                gang_restart,
                fanout.as_deref(),
            )?);
            // `--json`, `--outputs` and `--expect` reach the daemon as
            // reserved environment keys rather than new fields on
            // `IpcRequest::Run`: the executor re-decodes that request out of
            // the signed envelope to check authorization, so growing it
            // would break a mixed-version mesh (M19 §5.2). The origin daemon
            // reads them off and strips them before dispatch.
            let want = capture_wanted(json, outputs.as_deref(), expect.as_deref());
            env.extend(want.to_env_with(expect.as_deref()));
            // A race *is* a fan-out — to N nodes, with a stop rule. Reusing
            // the fan-out plan rather than inventing a second dispatch path
            // is what makes it survive the origin dying: the group is
            // already replicated and already adoptable (§10.3).
            let fanout = match race {
                Some(n) => {
                    if fanout.is_some() {
                        bail!(
                            "--race and --fanout are different questions. --fanout N runs on N \
                             nodes and waits for all of them; --race N runs on N nodes and keeps \
                             the first answer. Pick one."
                        );
                    }
                    if gang.is_some() {
                        bail!(
                            "--race and --gang cannot combine: a gang is N nodes that must all \
                             start together, and a race is N nodes of which all but one get \
                             cancelled."
                        );
                    }
                    if n < 2 {
                        bail!("--race {n} is not a race. Use 2 or more, or drop the flag.");
                    }
                    // No `--cacheable` requirement, though the first draft
                    // of this flag had one. `--cacheable` is already refused
                    // for fan-outs on purpose — each target is its own
                    // action there — and a race *is* a fan-out, so the two
                    // could never be given together. Weakening that rule to
                    // make room for this one would have traded a correct
                    // guard for a decorative one.
                    //
                    // What guards a dangerous race is what guards a
                    // dangerous fan-out: `--dry-run` prints the resolved
                    // targets and the blast radius before anything moves.
                    env.push((mesh_proto::REQ_RACE.to_string(), n.to_string()));
                    Some(n)
                }
                None => parse_fanout(fanout.as_deref())?,
            };
            let (priority, halt_on_fail) =
                parse_dispatch_opts(&priority, halt_on_fail.as_deref(), fanout, max_parallel)?;
            let inner = IpcRequest::Run {
                command: command.clone(),
                on,
                env,
                timeout_secs: timeout,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            };
            let started = std::time::Instant::now();
            let request = wrap_submission_with_key(inner, dry_run, explain_key)?;
            // The fallback decides on two things and two only: no daemon
            // on the socket, or a daemon that says no live node can run
            // this. Anything else — a denial, a policy, a bad request — is
            // an answer, and running it here anyway would be working
            // around the answer.
            let (mut stream, first) = match ctx.stream(request).await {
                Ok(mut stream) if fallback_local => {
                    let first: IpcResponse = match read_frame(&mut stream).await {
                        Ok(frame) => frame,
                        Err(e) => bail!("{e}"),
                    };
                    match &first {
                        IpcResponse::Error { message } if is_no_capacity(message) => {
                            return run_locally(&command, &user_env, timeout, message).await;
                        }
                        _ => (stream, Some(first)),
                    }
                }
                Ok(stream) => (stream, None),
                Err(e) if fallback_local && is_no_daemon(&e) => {
                    return run_locally(&command, &user_env, timeout, "no daemon on this machine")
                        .await;
                }
                Err(e) => return Err(e),
            };
            // `--json` suppresses the live log: the point of a row is that
            // it is the answer, and a JSON array with six hundred logs
            // interleaved above it is not machine-readable.
            let mut report = RunReport::default();
            let code = consume_job_stream_full(
                &mut stream,
                first,
                !json,
                json,
                race.is_some(),
                &mut report,
                runview::RunView::new(Some(&command), stream_lines),
            )
            .await?;
            if notify {
                let event = notify::job_event(
                    report.job.as_deref().unwrap_or("job"),
                    report.node.as_deref().unwrap_or("the mesh"),
                    &command,
                    report.state.unwrap_or(mesh_proto::JobState::Failed),
                    report.exit_code.or(Some(code)),
                    started.elapsed().as_secs(),
                );
                for note in notify::send(&ctx.config.notify, &event).await {
                    eprintln!("{}", crate::term::err().status(&format!("· {note}")));
                }
            }
            Ok(code)
        }
        // -- the fabric (M17 §3.1) ------------------------------------
        Command::Shell {
            node,
            reason,
            command,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            sessions::shell(&ctx, &node, &reason, command).await
        }
        Command::Sessions { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(SessionCmd::Ls {
                output: Output::Table,
            }) {
                SessionCmd::Ls { output } => {
                    let resp = ctx.ipc(IpcRequest::Sessions).await?;
                    let IpcResponse::Sessions(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_sessions(&rows, output == Output::Json)?;
                    Ok(0)
                }
                SessionCmd::Close { id } => {
                    let resp = ctx.ipc(IpcRequest::SessionClose { id }).await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("session {id} closed");
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
                SessionCmd::Policy(SessionPolicyCmd::Ls { output }) => {
                    let resp = ctx.ipc(IpcRequest::SessionPolicies).await?;
                    let IpcResponse::SessionPolicies(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_session_policies(&rows, output == Output::Json)?;
                    Ok(0)
                }
                SessionCmd::Policy(SessionPolicyCmd::Put {
                    id,
                    users,
                    nodes,
                    allow,
                    description,
                    expires_in,
                }) => {
                    let mut permissions = Vec::new();
                    for name in &allow {
                        match mesh_proto::SessionPermission::parse(name) {
                            Some(p) if !permissions.contains(&p) => permissions.push(p),
                            Some(_) => {}
                            None => bail!(
                                "{name:?} is not a session permission \
                                 (shell | shell-privileged)"
                            ),
                        }
                    }
                    let expires_at_micros = match expires_in.as_deref() {
                        Some(text) => {
                            let d = humantime::parse_duration(text)
                                .with_context(|| format!("{text:?} is not a duration"))?;
                            Some(
                                mesh_proto::now_micros()
                                    + u64::try_from(d.as_micros()).unwrap_or(u64::MAX),
                            )
                        }
                        None => None,
                    };
                    let mut policy = mesh_proto::SessionPolicy {
                        id,
                        description,
                        users,
                        nodes,
                        allow: permissions,
                        expires_at_micros,
                        created_at_micros: mesh_proto::now_micros(),
                        created_by: ctx.user_id(),
                        signature: Vec::new(),
                    };
                    policy.signature = ctx.sign(&policy.signing_bytes())?;
                    let name = policy.id.clone();
                    let resp = ctx
                        .ipc(IpcRequest::SessionPolicyPut {
                            policy: Box::new(policy),
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("session policy '{name}' written");
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
                SessionCmd::Policy(SessionPolicyCmd::Rm { id }) => {
                    let by = ctx.user_id();
                    let signature = ctx
                        .user
                        .sign(&mesh_proto::session_policy_remove_signing_bytes(&id, &by))
                        .to_vec();
                    let resp = ctx
                        .ipc(IpcRequest::SessionPolicyRemove {
                            id: id.clone(),
                            by,
                            signature,
                        })
                        .await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("session policy '{id}' retired");
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
            }
        }
        Command::Parallel {
            arg_file,
            on,
            env,
            timeout,
            halt_on_fail,
            resume,
            cpus,
            mem,
            disk,
            service,
            set,
            item_input,
            chunk,
            progress,
            dry_run,
            cacheable,
            rest,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if let Some(job) = &resume {
                if !rest.is_empty() {
                    bail!("--resume takes the command and items from the earlier run; drop them");
                }
                let Some((inner, _)) =
                    resume_request(&ctx, job, Capture::Inline, halt_on_fail.as_deref()).await?
                else {
                    return Ok(0);
                };
                let mut stream = ctx.stream(wrap_submission(inner, dry_run)).await?;
                return consume_parallel_stream(&mut stream, ParallelView::inline(progress)).await;
            }
            let (command, items) = if service.is_some() {
                // With --service the trailing words are items, not a
                // command: the script *is* the command.
                (String::new(), parallel_items(rest, arg_file.as_deref())?)
            } else {
                parse_parallel_input(rest, arg_file.as_deref())?
            };
            let mut job_env = parse_env(&env)?;
            job_env.extend(parse_requirements(cpus, mem.as_deref(), disk.as_deref())?.to_env());
            // C7 P3 — per item, which is where the milestone's headline
            // number lives: a map of five hundred items run twice is five
            // hundred separate actions, and an item whose command and
            // inputs have not moved has nothing to recompute.
            job_env.extend(cache_env(cacheable, None, false)?);
            job_env.extend(halt_env(halt_on_fail.as_deref())?);
            let inner = IpcRequest::Parallel {
                command,
                service,
                inputs: parse_env(&set)?,
                item_input,
                items,
                on,
                env: job_env,
                timeout_secs: timeout,
                chunk,
                capture: Capture::Inline,
                reduce: None,
                reduce_on: None,
            };
            let mut stream = ctx.stream(wrap_submission(inner, dry_run)).await?;
            consume_parallel_stream(&mut stream, ParallelView::inline(progress)).await
        }
        Command::Map {
            arg_file,
            on,
            env,
            timeout,
            halt_on_fail,
            resume,
            cpus,
            mem,
            disk,
            service,
            set,
            item_input,
            chunk,
            reduce,
            reduce_on,
            out,
            progress,
            cacheable,
            rest,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if let Some(job) = &resume {
                if !rest.is_empty() {
                    bail!("--resume takes the command and items from the earlier run; drop them");
                }
                let Some((inner, submitted)) =
                    resume_request(&ctx, job, Capture::Object, halt_on_fail.as_deref()).await?
                else {
                    return Ok(0);
                };
                let mut stream = ctx.stream(inner).await?;
                let view = ParallelView {
                    progress,
                    objects: true,
                    out: out.clone(),
                    fetched: Vec::new(),
                    job: None,
                };
                let (code, fetched, map_job) = consume_parallel_map(&mut stream, view).await?;
                if let Some(dir) = out {
                    fetch_outputs(&ctx, &dir, &submitted, &fetched).await?;
                    if let Some(job) = map_job {
                        mark_collected(&ctx, job).await;
                    }
                }
                return Ok(code);
            }
            let (command, items) = if service.is_some() {
                (String::new(), parallel_items(rest, arg_file.as_deref())?)
            } else {
                parse_parallel_input(rest, arg_file.as_deref())?
            };
            let mut job_env = parse_env(&env)?;
            job_env.extend(parse_requirements(cpus, mem.as_deref(), disk.as_deref())?.to_env());
            // C7 P3 — per item, which is where the milestone's headline
            // number lives: a map of five hundred items run twice is five
            // hundred separate actions, and an item whose command and
            // inputs have not moved has nothing to recompute.
            job_env.extend(cache_env(cacheable, None, false)?);
            job_env.extend(halt_env(halt_on_fail.as_deref())?);
            let submitted = items.clone();
            let mut stream = ctx
                .stream(IpcRequest::Parallel {
                    command,
                    service,
                    inputs: parse_env(&set)?,
                    item_input,
                    items,
                    on,
                    env: job_env,
                    timeout_secs: timeout,
                    chunk,
                    // The one difference between `map` and `parallel`.
                    capture: Capture::Object,
                    reduce,
                    reduce_on,
                })
                .await?;
            let view = ParallelView {
                progress,
                objects: true,
                out: out.clone(),
                fetched: Vec::new(),
                job: None,
            };
            let (code, fetched, map_job) = consume_parallel_map(&mut stream, view).await?;
            // `--out` is the deliberate exception to "output stays where it
            // was produced": the operator asked for the bytes here, and
            // this is where they get fetched — after the run, not during it.
            if let Some(dir) = out {
                fetch_outputs(&ctx, &dir, &submitted, &fetched).await?;
                if let Some(job) = map_job {
                    mark_collected(&ctx, job).await;
                }
            }
            Ok(code)
        }
        Command::Objects { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(ObjectsCmd::Ls) {
                ObjectsCmd::Ls => {
                    let resp = ctx.ipc(IpcRequest::Objects).await?;
                    let IpcResponse::Objects(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_objects(&rows, output == Output::Json)?;
                    Ok(0)
                }
                ObjectsCmd::Get { object, out } => {
                    let id = resolve_object_id(&ctx, &object).await?;
                    let mut stream = ctx
                        .stream(IpcRequest::ObjectGet { id, hints: vec![] })
                        .await?;
                    let size = drain_object(&mut stream, out.as_deref()).await?;
                    if out.is_some() {
                        eprintln!("{} — {}", id.short(), format::bytes(size));
                    }
                    Ok(0)
                }
            }
        }
        Command::Jobs { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(JobsCmd::Ls {
                state: None,
                contains: None,
                limit: 50,
                offset: 0,
            }) {
                JobsCmd::Ls {
                    state,
                    contains,
                    limit,
                    offset,
                } => {
                    let resp = ctx
                        .ipc(IpcRequest::JobsQuery {
                            query: Box::new(mesh_proto::JobsQuery {
                                state,
                                contains,
                                limit,
                                offset,
                                ..Default::default()
                            }),
                        })
                        .await?;
                    let IpcResponse::JobsPage(page) = resp else {
                        return unexpected(resp);
                    };
                    format::print_jobs_page(&page, output == Output::Json)?;
                    Ok(0)
                }
                JobsCmd::Show { job } => {
                    let job_id = resolve_job(&ctx, &job).await?;
                    let resp = ctx.ipc(IpcRequest::JobShow { job_id }).await?;
                    let IpcResponse::JobShow(row) = resp else {
                        return unexpected(resp);
                    };
                    format::print_job(&row, output == Output::Json)?;
                    if output != Output::Json {
                        format::print_deep_link(
                            format::DeepLinkKind::Job,
                            &job_id.to_display_string(),
                        );
                    }
                    Ok(0)
                }
                JobsCmd::Clear { yes } => {
                    // Unlike `cache purge`, which costs re-execution and
                    // nothing else, this one is not recoverable: the record
                    // of what ran is the thing being deleted. Hence --yes,
                    // and hence admin-only at the daemon.
                    if !yes {
                        bail!(
                            "`jobs clear` deletes finished jobs for everyone on the mesh, permanently — pass --yes"
                        );
                    }
                    let before_micros = mesh_proto::now_micros();
                    let signature = ctx
                        .user
                        .sign(&mesh_proto::jobs_clear_signing_bytes(
                            before_micros,
                            &ctx.user_id(),
                        ))
                        .to_vec();
                    let resp = ctx
                        .ipc(IpcRequest::JobsClear {
                            before_micros,
                            by: ctx.user_id(),
                            signature,
                        })
                        .await?;
                    match resp {
                        IpcResponse::Notice { message } => {
                            println!("{message}");
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
                JobsCmd::Explain { job, output } => {
                    let job_id = resolve_job(&ctx, &job).await?;
                    let resp = ctx.ipc(IpcRequest::JobExplain { job_id }).await?;
                    let IpcResponse::JobExplain(view) = resp else {
                        return unexpected(resp);
                    };
                    format::print_job_explain(&view, output == Output::Json)?;
                    if output != Output::Json {
                        format::print_deep_link(
                            format::DeepLinkKind::Job,
                            &job_id.to_display_string(),
                        );
                    }
                    Ok(0)
                }
                JobsCmd::Cancel { job, force } => {
                    let job_id = resolve_job(&ctx, &job).await?;
                    if force {
                        let by = ctx.user_id();
                        let signature = ctx
                            .user
                            .sign(&mesh_proto::job_cancel_force_signing_bytes(&job_id, &by))
                            .to_vec();
                        let resp = ctx
                            .ipc(IpcRequest::JobCancelForce {
                                job_id,
                                by,
                                signature,
                            })
                            .await?;
                        return match resp {
                            IpcResponse::Notice { message } => {
                                println!("{message}");
                                Ok(0)
                            }
                            other => unexpected(other),
                        };
                    }
                    let resp = ctx.ipc(IpcRequest::JobCancel { job_id }).await?;
                    match resp {
                        IpcResponse::Ok => {
                            println!("cancel requested for {}", job_id.short());
                            Ok(0)
                        }
                        other => unexpected(other),
                    }
                }
                JobsCmd::Logs { job, follow } => {
                    let job_id = resolve_job(&ctx, &job).await?;
                    let mut stream = ctx.stream(IpcRequest::JobLogs { job_id, follow }).await?;
                    consume_job_stream(&mut stream, false, false).await
                }
                JobsCmd::Wait {
                    job,
                    then,
                    timeout,
                    notify,
                } => {
                    let job_id = resolve_job(&ctx, &job).await?;
                    jobs_wait(&ctx, job_id, then.as_deref(), timeout.as_deref(), notify).await
                }
            }
        }
        Command::Script { cmd } => match cmd {
            ScriptCmd::Inspect { path, json } => {
                let (spec, from) = read_script_spec(std::path::Path::new(&path))?;
                if json {
                    match spec.contract_json() {
                        Some(c) => println!("{c}"),
                        None => println!("null"),
                    }
                    return Ok(0);
                }
                print_script_spec(&path, &spec, from);
                Ok(0)
            }
            ScriptCmd::Definitions {
                path,
                stdout,
                force,
            } => {
                let script = std::path::Path::new(&path);
                // Read the script alone. Reading the definitions file too
                // and writing it back would be a no-op that reports success,
                // which is not what "generate this file" should mean.
                let source = std::fs::read_to_string(script)
                    .with_context(|| format!("cannot read {path}"))?;
                let text = mesh_core::script_spec::ScriptSpec::read(&source).to_definitions();
                if stdout {
                    print!("{text}");
                    return Ok(0);
                }
                let target = mesh_core::script_spec::definitions_path(&path);
                if std::path::Path::new(&target).exists() && !force {
                    bail!("{target} already exists — pass --force to overwrite it");
                }
                std::fs::write(&target, &text).with_context(|| format!("cannot write {target}"))?;
                println!("wrote {target}");
                Ok(0)
            }
        },
        Command::Add {
            path,
            name,
            entry,
            runtime,
        } => add_script(&cli.global, &path, name, entry, runtime).await,
        Command::Push { path } => {
            let ctx = Ctx::new(&cli.global)?;
            // Resolve to an absolute path since the daemon reads it.
            let abs =
                std::fs::canonicalize(&path).with_context(|| format!("cannot access {path}"))?;
            let resp = ctx
                .ipc(IpcRequest::Push {
                    path: abs.to_string_lossy().into_owned(),
                })
                .await?;
            match resp {
                IpcResponse::Pushed {
                    bundle_id,
                    file_count,
                    chunk_count,
                    total_size,
                } => {
                    println!(
                        "pushed {bundle_id}\n  {file_count} file(s), {chunk_count} chunk(s), {} bytes",
                        total_size
                    );
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        Command::Content { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(ContentCmd::Status) {
                ContentCmd::Status => {
                    let resp = ctx.ipc(IpcRequest::ContentStatus).await?;
                    let IpcResponse::ContentStatus(view) = resp else {
                        return unexpected(resp);
                    };
                    format::print_content_status(&view, output == Output::Json)?;
                    Ok(0)
                }
                ContentCmd::Prune {
                    dry_run,
                    aggressive,
                } => {
                    let resp = ctx
                        .ipc(IpcRequest::ContentPrune {
                            dry_run,
                            aggressive,
                        })
                        .await?;
                    let IpcResponse::ContentPrune(view) = resp else {
                        return unexpected(resp);
                    };
                    format::print_content_prune(&view, output == Output::Json)?;
                    Ok(0)
                }
            }
        }
        Command::Disk { output } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::DiskReport).await?;
            let IpcResponse::DiskReport(rows) = resp else {
                return unexpected(resp);
            };
            format::print_disk(&rows, output == Output::Json)?;
            // A node under its floor is a thing to notice in a script, not
            // only on a screen: exit 1 so `meshnode disk && deploy` stops.
            Ok(i32::from(rows.iter().any(|r| r.state == "low")))
        }
        Command::Bundles { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            match cmd.unwrap_or(BundlesCmd::Ls) {
                BundlesCmd::Ls => {
                    let resp = ctx.ipc(IpcRequest::Bundles).await?;
                    let IpcResponse::Bundles(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_bundles(&rows, output == Output::Json)?;
                    Ok(0)
                }
                BundlesCmd::Prefetch { bundle, on } => {
                    let bundle_id = resolve_bundle_ref(&ctx, &bundle).await?;
                    run_prefetch(&ctx, bundle_id, on, output == Output::Json).await
                }
                BundlesCmd::Show { bundle } => {
                    let bundle_id = resolve_bundle(&ctx, &bundle).await?;
                    let resp = ctx.ipc(IpcRequest::BundleFiles { bundle_id }).await?;
                    let IpcResponse::BundleFiles(files) = resp else {
                        return unexpected(resp);
                    };
                    if output == Output::Json {
                        println!("{}", serde_json::to_string_pretty(&files)?);
                    } else {
                        println!("{:<10} {:<6} {:<6} PATH", "SIZE", "MODE", "EXEC");
                        for f in &files {
                            println!(
                                "{:<10} {:<6o} {:<6} {}{}",
                                f.size,
                                f.mode & 0o7777,
                                if f.executable { "yes" } else { "-" },
                                f.path,
                                f.symlink
                                    .as_deref()
                                    .map(|s| format!(" -> {s}"))
                                    .unwrap_or_default()
                            );
                        }
                    }
                    Ok(0)
                }
                BundlesCmd::Cat { bundle, path } => {
                    let bundle_id = resolve_bundle(&ctx, &bundle).await?;
                    // Streamed rather than read in one frame, so this works
                    // on a payload as well as on a contract — a release
                    // binary is twenty-five times the single-frame cap, and
                    // `> meshnode` is the obvious way to get one out.
                    let mut stream = ctx
                        .stream(IpcRequest::BundleFileFetch {
                            bundle_id,
                            path: path.clone(),
                        })
                        .await?;
                    use std::io::Write as _;
                    let mut out = std::io::stdout().lock();
                    let mut got: u64 = 0;
                    loop {
                        let resp: IpcResponse = match read_frame(&mut stream).await {
                            Ok(r) => r,
                            Err(mesh_proto::ProtoError::Io(e)) => {
                                bail!("{path} was cut short after {got} bytes: {e}")
                            }
                            Err(e) => return Err(e.into()),
                        };
                        match resp {
                            IpcResponse::BundleFileChunk { data } => {
                                got += data.len() as u64;
                                out.write_all(&data)?;
                            }
                            IpcResponse::BundleFileDone { size } => {
                                out.flush()?;
                                // The terminal frame is the whole reason
                                // this is not just "write what arrives":
                                // half a binary written to a file looks
                                // exactly like a whole one until it runs.
                                if got != size {
                                    bail!(
                                        "{path} ended after {got} bytes, but its manifest says \
                                         {size}"
                                    );
                                }
                                return Ok(0);
                            }
                            IpcResponse::Error { message } => bail!("{message}"),
                            IpcResponse::Denied { message } => {
                                bail!("permission denied: {message}")
                            }
                            other => return unexpected(other),
                        }
                    }
                }
            }
        }
        Command::Service { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            run_service_cmd(&ctx, cmd.unwrap_or(ServiceCmd::Ls { output })).await
        }
        Command::Dns {
            cmd: DnsCmd::Install { address, dry_run },
        } => {
            let addr: std::net::SocketAddr = address
                .parse()
                .map_err(|_| anyhow!("--address takes host:port, e.g. 127.0.0.1:8600"))?;
            if dry_run {
                let plan =
                    mesh_core::dnssetup::plan(addr, true, mesh_core::dnssetup::Legacy::detect());
                for (path, body) in &plan.files {
                    println!("# {}\n{body}", path.display());
                }
                for path in &plan.removals {
                    println!("$ rm -f {}", path.display());
                }
                for command in &plan.commands {
                    println!("$ {}", command.join(" "));
                }
                return Ok(0);
            }
            for line in mesh_core::dnssetup::install(addr).map_err(|e| anyhow!("{e}"))? {
                println!("{line}");
            }
            Ok(0)
        }
        Command::Registry {
            service,
            dns,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if dns {
                let resp = ctx.ipc(IpcRequest::DnsStatus).await?;
                let IpcResponse::DnsStatus(v) = resp else {
                    return unexpected(resp);
                };
                format::print_dns_status(&v, output == Output::Json)?;
                return Ok(0);
            }
            match service {
                None => {
                    let resp = ctx.ipc(IpcRequest::Registry).await?;
                    let IpcResponse::Registry(rows) = resp else {
                        return unexpected(resp);
                    };
                    let dns = match ctx.ipc(IpcRequest::DnsStatus).await {
                        Ok(IpcResponse::DnsStatus(v)) => Some(*v),
                        _ => None,
                    };
                    format::print_registry(&rows, dns.as_ref(), output == Output::Json)?;
                    Ok(0)
                }
                Some(service) => {
                    let resp = ctx
                        .ipc(IpcRequest::RegistryShow {
                            service: service.clone(),
                        })
                        .await?;
                    let IpcResponse::RegistryShow(rows) = resp else {
                        return unexpected(resp);
                    };
                    format::print_registry_service(&service, &rows, output == Output::Json)?;
                    Ok(0)
                }
            }
        }
        Command::Shed { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            // Every set is read-modify-write over the *current* record, so
            // `shed submissions` does not silently lift a reconcile shed
            // somebody else set ten minutes ago. The wire carries the
            // absolute state; the verb is what turns "one switch" into it.
            let current = match ctx.ipc(IpcRequest::ShedStatus).await? {
                IpcResponse::Shed(v) => *v,
                other => return unexpected(other),
            };
            let (submissions, reconcile, reason, output) = match cmd {
                ShedCmd::Status { output } => {
                    format::print_shed(&current, output == Output::Json)?;
                    return Ok(0);
                }
                ShedCmd::Submissions { off, reason } => (
                    !off,
                    current.reconcile,
                    reason.unwrap_or(current.reason),
                    Output::Table,
                ),
                ShedCmd::Reconcile { off, reason } => (
                    current.submissions,
                    !off,
                    reason.unwrap_or(current.reason),
                    Output::Table,
                ),
                ShedCmd::Clear => (false, false, String::new(), Output::Table),
            };
            let mut shed = mesh_proto::shed::Shed {
                submissions,
                reconcile,
                reason,
                created_by: ctx.user_id(),
                created_at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            shed.signature = ctx.sign(&shed.signing_bytes())?;
            let sheds = shed.sheds();
            let resp = ctx
                .ipc(IpcRequest::ShedSet {
                    shed: Box::new(shed),
                })
                .await?;
            let IpcResponse::Shed(view) = resp else {
                return unexpected(resp);
            };
            format::print_shed(&view, output == Output::Json)?;
            // Said at the moment somebody flips it on, not only in a doc
            // they may never open: this is the gate that fails open, and an
            // operator shedding load on a half-upgraded mesh has to know
            // which half they actually shed.
            if sheds && let Some(w) = view.readiness.warning() {
                eprintln!();
                eprintln!("{}", term::warning_line(&w.to_string()));
            }
            Ok(0)
        }
        Command::Volumes { cmd, node, output } => match cmd {
            None => {
                let ctx = Ctx::new(&cli.global)?;
                let resp = ctx.ipc(IpcRequest::Volumes { node }).await?;
                let IpcResponse::Volumes(rows) = resp else {
                    return unexpected(resp);
                };
                format::print_volumes(&rows, output == Output::Json)?;
                Ok(0)
            }
            Some(VolumeCmd::Create {
                name,
                capacity_mb,
                params,
                read_only,
            }) => {
                // Refused here as well as on the node: the person typing it
                // is standing right here, and `key=value` is a shape a
                // message can name rather than a plugin discovering.
                let mut parsed: Vec<(String, String)> = Vec::with_capacity(params.len());
                for p in &params {
                    let Some((k, v)) = p.split_once('=') else {
                        bail!("--param {p:?} is not key=value");
                    };
                    let (k, v) = (k.trim(), v.trim());
                    if k.is_empty() {
                        bail!("--param {p:?} has an empty key");
                    }
                    parsed.push((k.to_string(), v.to_string()));
                }
                let ctx = Ctx::new(&cli.global)?;
                let resp = ctx
                    .ipc(IpcRequest::VolumeCreate {
                        name: name.clone(),
                        capacity_mb: capacity_mb.unwrap_or(0),
                        params: parsed,
                        read_only,
                    })
                    .await?;
                let IpcResponse::VolumeCreated(row) = resp else {
                    return unexpected(resp);
                };
                format::print_volumes(std::slice::from_ref(&row), output == Output::Json)?;
                Ok(0)
            }
            Some(VolumeCmd::Rm { name, force }) => {
                let ctx = Ctx::new(&cli.global)?;
                let resp = ctx.ipc(IpcRequest::VolumeRemove { name, force }).await?;
                let IpcResponse::VolumeRemoved { message } = resp else {
                    return unexpected(resp);
                };
                println!("{message}");
                Ok(0)
            }
        },
        Command::Runtimes { node, output } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Runtimes { node }).await?;
            let IpcResponse::Runtimes(view) = resp else {
                return unexpected(resp);
            };
            format::print_runtimes(&view, output == Output::Json)?;
            Ok(0)
        }
        Command::Ports { node, output } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Ports { node }).await?;
            let IpcResponse::Ports(view) = resp else {
                return unexpected(resp);
            };
            format::print_ports(&view, output == Output::Json)?;
            Ok(0)
        }
        Command::Schedule { cmd, output } => {
            run_schedule(&cli.global, cmd.unwrap_or(ScheduleCmd::Ls { output })).await
        }
        // Un-gated 2026-09-12 (D-N1, `docs/N1-N6-THE-NOMAD-TRACK.md` §16).
        // The Focus sweep caught `deployment` with the orchestrator verbs;
        // it turned out to be the spine of what the product is growing
        // into rather than part of what it was pruning. The other five
        // stay gated, and `GO-TO-MARKET.md` G6 still governs them.
        Command::Deployment { cmd, output } => {
            run_deployment(&cli.global, cmd.unwrap_or(DeploymentCmd::Ls { output })).await
        }
        Command::Facts {
            subject,
            collector,
            cmd,
            output,
        } => {
            run_facts(
                &cli.global,
                cmd.unwrap_or(FactsCmd::Show {
                    subject,
                    collector,
                    output,
                }),
            )
            .await
        }
        Command::Collectors { cmd, output } => {
            run_collectors(&cli.global, cmd.unwrap_or(CollectorsCmd::Ls { output })).await
        }
        Command::Results {
            group,
            failed,
            output,
            cmd,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if let Some(ResultsCmd::Diff {
                before,
                after,
                all,
                output,
            }) = cmd
            {
                return results_diff(&ctx, &before, &after, all, output == Output::Json).await;
            }
            let Some(group) = group else {
                bail!("give a fan-out group id, or `history diff BEFORE AFTER`");
            };
            print_fanout_results(&ctx, &group, failed, output).await
        }
        Command::Top {
            output,
            follow,
            interval,
            devices,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if follow && output == Output::Json {
                bail!(
                    "--follow redraws a table; for a stream of JSON, poll `meshnode top --output json`"
                );
            }
            top(
                &ctx,
                output == Output::Json,
                follow,
                interval.max(1),
                devices,
            )
            .await
        }
        Command::Totals { output } => {
            let ctx = Ctx::new(&cli.global)?;
            let resp = ctx.ipc(IpcRequest::Totals).await?;
            let IpcResponse::Totals(totals) = resp else {
                return unexpected(resp);
            };
            format::print_totals(&totals, output == Output::Json)?;
            Ok(0)
        }
        Command::Sources { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            run_sources_cmd(&ctx, cmd.unwrap_or(SourcesCmd::Ls { output })).await
        }
        Command::Users { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            run_users_cmd(&ctx, cmd.unwrap_or(UsersCmd::Ls { output })).await
        }
        Command::Login { provider, wait } => {
            let ctx = Ctx::new(&cli.global)?;
            run_login(&ctx, &provider, &wait).await
        }
        Command::Oidc { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            run_oidc_cmd(&ctx, cmd).await
        }
        Command::ServiceAccounts { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            run_service_accounts_cmd(&ctx, cmd.unwrap_or(ServiceAccountsCmd::Ls { output })).await
        }
        Command::Grants { cmd, output } => {
            let ctx = Ctx::new(&cli.global)?;
            run_grants_cmd(&ctx, cmd.unwrap_or(GrantsCmd::Ls { output })).await
        }
        Command::Secrets { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            run_secrets_cmd(&ctx, cmd).await
        }
        Command::Audit {
            user,
            action,
            since,
            limit,
            output,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            let since_micros = match since {
                Some(s) => {
                    let dur = humantime::parse_duration(&s)
                        .context("invalid --since (try 24h, 30m, 7d)")?;
                    Some(
                        mesh_proto::now_micros()
                            .saturating_sub(u64::try_from(dur.as_micros()).unwrap_or(u64::MAX)),
                    )
                }
                None => None,
            };
            let resp = ctx
                .ipc(IpcRequest::AuditQuery {
                    user,
                    action,
                    since_micros,
                    limit,
                })
                .await?;
            let IpcResponse::Audit(entries) = resp else {
                return unexpected(resp);
            };
            // Actor type is a projection over state (E1-P4), so the join
            // happens here rather than widening the audit response. Only
            // for the table: `--output json`/`jsonl` must stay byte-stable
            // for whatever is parsing it, and a SIEM pipeline that grew a
            // new field overnight is a bad surprise.
            let machines: Vec<mesh_proto::UserId> = if output == "table" {
                match ctx.ipc(IpcRequest::ServiceAccounts).await {
                    Ok(IpcResponse::ServiceAccounts(rows)) => {
                        rows.iter().map(|r| r.user_id).collect()
                    }
                    // A viewer may read audit and not service accounts.
                    // Losing the marker is not worth losing the audit.
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            format::print_audit_with_actors(&entries, &output, &machines)?;
            Ok(0)
        }
        Command::Config { cmd } => {
            let config = resolve_config(&cli.global, Overrides::default())?;
            match cmd {
                ConfigCmd::Check => {
                    let notes = config.check()?;
                    println!("configuration ok");
                    for n in notes {
                        println!("  note: {n}");
                    }
                    Ok(0)
                }
                ConfigCmd::Show => {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                    Ok(0)
                }
                ConfigCmd::Paths { output } => {
                    format::print_paths(&config.layout().describe(), output == Output::Json)?;
                    Ok(0)
                }
            }
        }
        Command::History { cmd } => {
            let ctx = Ctx::new(&cli.global)?;
            run_history_cmd(
                &ctx,
                // Jobs, not runs (DOC-11): only the retired workflow
                // engine ever wrote a run, so the bare verb answered
                // every mesh with an empty table.
                cmd.unwrap_or(HistoryCmd::Jobs {
                    state: None,
                    node: None,
                    since: None,
                    until: None,
                    grep: None,
                    limit: 20,
                    offset: 0,
                    output: Output::Table,
                }),
            )
            .await
        }
        Command::Reset => {
            let ctx = Ctx::new(&cli.global)?;
            reset::run(&ctx).await
        }
        Command::Drain {
            move_pending,
            deadline,
            until,
            for_,
            reason,
            lift,
        } => {
            let ctx = Ctx::new(&cli.global)?;
            if lift || until.is_some() || for_.is_some() {
                return drain_timed(
                    &ctx,
                    until.as_deref(),
                    for_.as_deref(),
                    reason,
                    lift,
                    move_pending,
                )
                .await;
            }
            let request = match deadline.as_deref() {
                Some(text) => IpcRequest::DrainDeadline {
                    move_pending,
                    deadline_secs: humantime::parse_duration(text)
                        .with_context(|| format!("--deadline {text:?} is not a duration"))?
                        .as_secs(),
                },
                None => IpcRequest::Drain { move_pending },
            };
            let mut stream = ctx.stream(request).await?;
            eprintln!(
                "draining{} — running jobs finish, new work is refused, deployment instances \
                 move{}…",
                if move_pending {
                    " (moving queued jobs)"
                } else {
                    ""
                },
                match deadline.as_deref() {
                    Some(text) => format!("; whatever is left after {text} is stopped"),
                    None => String::new(),
                }
            );
            let mut last = String::new();
            loop {
                let resp: IpcResponse = match read_frame(&mut stream).await {
                    Ok(r) => r,
                    // The daemon exits right after the final frame; a
                    // closed stream after `Ok` is the expected ending.
                    Err(mesh_proto::ProtoError::Io(_)) => break,
                    Err(e) => return Err(e.into()),
                };
                match resp {
                    IpcResponse::DrainProgress {
                        running,
                        queued,
                        moved,
                        devices_held,
                    } => {
                        let line = format!(
                            "  {running} running, {queued} queued{}{}",
                            if moved > 0 {
                                format!(", {moved} moved")
                            } else {
                                String::new()
                            },
                            // The question somebody draining a GPU box for
                            // a card swap actually has.
                            if devices_held > 0 {
                                format!(", {devices_held} device(s) still held")
                            } else {
                                String::new()
                            }
                        );
                        if line != last {
                            eprintln!("{line}");
                            last = line;
                        }
                    }
                    IpcResponse::Ok => {
                        println!("drained — the daemon is exiting cleanly.");
                        return Ok(0);
                    }
                    IpcResponse::Denied { message } => bail!("permission denied: {message}"),
                    IpcResponse::Error { message } => bail!("{message}"),
                    other => bail!("unexpected drain frame: {other:?}"),
                }
            }
            println!("drained — the daemon is exiting cleanly.");
            Ok(0)
        }
        Command::Support {
            cmd:
                SupportCmd::Bundle {
                    output,
                    since,
                    limit,
                },
        } => {
            let since = humantime::parse_duration(&since).context("invalid --since")?;
            let config = resolve_config(&cli.global, Overrides::default())?;
            // The user key is loaded the way `doctor` loads it, and for the
            // same reason: a bundle has to be collectable on a node whose
            // daemon is down, and that path needs no socket.
            let user = mesh_core::identity::load_cli_user(
                &config.data_dir,
                cli.global.user_identity.as_deref(),
            )
            .context("loading user identity")?;
            // Failing to reach the daemon is a *finding* here, not a stop:
            // it goes in the bundle and everything else is still collected.
            let ctx = Ctx::new(&cli.global).map_err(|e| e.to_string());
            let since_micros = std::time::SystemTime::now()
                .checked_sub(since)
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_micros() as u64);
            let bundle = support::collect(
                ctx.as_ref().map_err(|e| e.clone()),
                &config,
                &user,
                since_micros,
                limit,
            )
            .await;
            let path = support::write(&bundle, output)?;
            support::print_summary(&bundle, &path);
            Ok(0)
        }
        Command::Doctor { output, fix } => {
            let config = resolve_config(&cli.global, Overrides::default())?;
            let user = mesh_core::identity::load_cli_user(
                &config.data_dir,
                cli.global.user_identity.as_deref(),
            )
            .context("loading user identity")?;
            let checks = mesh_core::doctor::run(&config, &user).await;
            if !fix {
                if output == Output::Json {
                    println!("{}", serde_json::to_string_pretty(&checks)?);
                    return Ok(worst_verdict(&checks));
                }
                return Ok(print_doctor(&checks));
            }
            let fixes = mesh_core::doctor::fix(&config, &user, &checks).await;
            // Then look again: the verdict is what the node is like *after*
            // the fixes, not a re-print of what prompted them.
            let after = mesh_core::doctor::run(&config, &user).await;
            if output == Output::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "before": checks,
                        "fixes": fixes,
                        "after": after,
                    }))?
                );
                return Ok(worst_verdict(&after));
            }
            format::print_doctor_fixes(&fixes);
            println!();
            Ok(print_doctor(&after))
        }
        Command::Backup { out } => {
            let config = resolve_config(&cli.global, Overrides::default())?;
            let out = out.unwrap_or_else(|| {
                format!("meshware-backup-{}.mwb", config.node_name.replace('/', "_"))
            });
            // Absolute path: with a daemon, *it* writes the file.
            let abs = if std::path::Path::new(&out).is_absolute() {
                std::path::PathBuf::from(&out)
            } else {
                std::env::current_dir()?.join(&out)
            };
            // Daemon first (the store is locked while it runs); offline
            // fallback only when no daemon answers on the socket — a
            // daemon-side refusal (Denied/Error) surfaces as-is.
            let ctx = Ctx::new(&cli.global)?;
            let daemon_resp = mesh_core::ipc::request(
                &ctx.socket(),
                &ctx.user,
                IpcRequest::Backup {
                    path: abs.to_string_lossy().into_owned(),
                },
            )
            .await;
            let (path, ops, bytes) = match daemon_resp {
                Ok(IpcResponse::BackupDone { path, ops, bytes }) => (path, ops, bytes),
                Ok(IpcResponse::Denied { message }) => bail!("permission denied: {message}"),
                Ok(IpcResponse::Error { message }) => bail!("{message}"),
                Ok(other) => return unexpected(other),
                Err(_) => {
                    let r = mesh_core::backup::offline_backup(&config.data_dir, &abs)
                        .context("offline backup (no daemon reachable)")?;
                    (r.path.display().to_string(), r.ops, r.bytes)
                }
            };
            println!("backup written: {path} ({ops} ops, {bytes} bytes)");
            println!("note: it contains this node's private keys — treat it like a key file.");
            Ok(0)
        }
        Command::Restore { file, yes } => {
            if !yes {
                bail!(
                    "restore writes identity keys and mesh state into {} — re-run with --yes",
                    resolve_config(&cli.global, Overrides::default())?
                        .data_dir
                        .display()
                );
            }
            let config = resolve_config(&cli.global, Overrides::default())?;
            let doc = mesh_core::backup::read_backup(std::path::Path::new(&file))?;
            eprintln!(
                "restoring mesh '{}' (backup of node '{}', {} ops)…",
                doc.mesh_name, doc.node_name, doc.op_count
            );
            let report = mesh_core::backup::restore(std::path::Path::new(&file), &config.data_dir)?;
            println!(
                "restored mesh '{}' — {} ops replayed, {} member record(s)",
                report.mesh_name, report.ops, report.members
            );
            // The key comes back in the form it was in (ENG-14), and the
            // form decides what the operator has to do next.
            match report.user_key {
                "sealed" => println!(
                    "{}",
                    term::out().prose(
                        "the admin key came back passphrase-protected, as it was: \
                     `meshnode identity unlock` asks for the same passphrase"
                    )
                ),
                "hardware" => println!(
                    "the admin key came back as a reference to a hardware key: the private half \
                     never left the token, so plug it in on this machine"
                ),
                "absent" => println!("the backup held no admin key for this node"),
                _ => {}
            }
            // ENG-27: the one thing in a backup no peer could have given
            // back, so worth a line saying it came back.
            if report.volumes > 0 {
                println!(
                    "{} provisioned volume(s) restored — the directories themselves are on \
                     this machine's disks, unchanged",
                    report.volumes
                );
            }
            // M31 §19 Q5: say it here rather than letting somebody find out
            // when nothing exports. A restored mesh must not silently
            // re-push history into a live warehouse.
            if report.sinks_held > 0 {
                println!(
                    "{}",
                    term::out().prose(&format!(
                        "\n{} sink(s) are held and will export nothing.\n\
                     A restored mesh must not silently re-push history into a live warehouse,\n\
                     and a forked one must not push into the warehouse it forked away from.\n\
                     Review them with `meshnode sinks`, then `meshnode sinks resume`.",
                        report.sinks_held
                    ))
                );
            }
            println!("\n{}", mesh_core::backup::RESTORE_HOLD_NOTE);
            println!(
                "{}",
                term::out().prose("\nrun `meshnode up` to start the node.")
            );
            Ok(0)
        }
        Command::Completion { shell } => {
            // `full_command`, not `cli_command`: a verb the greeting does
            // not list still completes (D-L1.3), and `clap_complete` drops
            // hidden subcommands on the floor.
            let mut cmd = full_command();
            clap_complete::generate(shell, &mut cmd, "meshnode", &mut std::io::stdout());
            Ok(0)
        }
        Command::Manpage { dir } => {
            // Likewise: `clap_mangen` skips hidden subcommands, and a
            // shipped command with no man page is a command that was
            // quietly removed.
            let cmd = full_command();
            match dir {
                Some(dir) => {
                    std::fs::create_dir_all(&dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                    clap_mangen::generate_to(cmd, &dir)
                        .with_context(|| format!("writing man pages to {}", dir.display()))?;
                    let n = std::fs::read_dir(&dir)
                        .map(|d| d.flatten().count())
                        .unwrap_or(0);
                    eprintln!("wrote {n} man pages to {}", dir.display());
                }
                None => {
                    use std::io::Write as _;
                    let mut out = Vec::new();
                    clap_mangen::Man::new(cmd)
                        .render(&mut out)
                        .context("rendering the man page")?;
                    std::io::stdout().write_all(&out)?;
                }
            }
            Ok(0)
        }
        Command::Upgrade {
            self_,
            on,
            batch,
            status,
            publish,
            publish_sig,
            releases,
            plans,
            trust_key,
            key_label,
            untrust_key,
            keys,
            cancel,
            check,
            rollback,
            version,
            from_file,
            minisig,
            cosign,
            cosign_publish,
            release_base,
            release_key,
            no_drain,
            move_pending,
            force,
            yes,
        } => {
            if status {
                let ctx = Ctx::new(&cli.global)?;
                let IpcResponse::Nodes(rows) = ctx.ipc(IpcRequest::Nodes).await? else {
                    bail!("unexpected response to nodes");
                };
                format::print_fleet_versions(&rows, version.as_deref())?;
                return Ok(0);
            }
            if releases {
                let ctx = Ctx::new(&cli.global)?;
                let IpcResponse::Releases(rows) = ctx.ipc(IpcRequest::Releases).await? else {
                    bail!("unexpected response to releases");
                };
                format::print_releases(&rows)?;
                return Ok(0);
            }
            if let Some(binary) = publish {
                let ctx = Ctx::new(&cli.global)?;
                let Some(version) = version.clone() else {
                    bail!("pass --version: a release names the version it is");
                };
                return publish_release(&ctx, &binary, publish_sig.as_deref(), &version).await;
            }
            if let Some(sig_file) = cosign_publish {
                let ctx = Ctx::new(&cli.global)?;
                let Some(version) = version.clone() else {
                    bail!("pass --version: a co-signature names the release it is about");
                };
                return publish_cosignature(&ctx, &sig_file, &version).await;
            }
            if keys {
                let ctx = Ctx::new(&cli.global)?;
                let IpcResponse::ReleaseKeys(view) = ctx.ipc(IpcRequest::ReleaseKeys).await? else {
                    bail!("unexpected response to release keys");
                };
                format::print_release_keys(&view)?;
                return Ok(0);
            }
            if let Some(source) = trust_key {
                let ctx = Ctx::new(&cli.global)?;
                return trust_release_key(&ctx, &source, key_label.as_deref()).await;
            }
            if let Some(pubkey) = untrust_key {
                let ctx = Ctx::new(&cli.global)?;
                let pubkey = pubkey.trim().to_string();
                let at_micros = mesh_proto::now_micros();
                let signature = ctx
                    .user
                    .sign(&mesh_proto::release_key_revoke_signing_bytes(
                        &pubkey,
                        &ctx.user_id(),
                        at_micros,
                    ))
                    .to_vec();
                return match ctx
                    .ipc(IpcRequest::ReleaseKeyRevoke {
                        pubkey: pubkey.clone(),
                        by: ctx.user_id(),
                        at_micros,
                        signature,
                    })
                    .await?
                {
                    IpcResponse::Ok => {
                        println!("withdrew trust in {pubkey}");
                        println!(
                            "Nodes stop accepting releases signed by it as the revocation \
                             reaches them. Releases already installed are untouched."
                        );
                        Ok(0)
                    }
                    other => bail!("{other:?}"),
                };
            }
            if plans {
                let ctx = Ctx::new(&cli.global)?;
                let IpcResponse::UpgradePlans(rows) = ctx.ipc(IpcRequest::UpgradePlans).await?
                else {
                    bail!("unexpected response to upgrade plans");
                };
                format::print_upgrade_plans(&rows)?;
                return Ok(0);
            }
            if let Some(plan) = cancel {
                let ctx = Ctx::new(&cli.global)?;
                let plan_id = mesh_proto::UpgradeId::parse(plan.trim())
                    .context("invalid plan id (expected upg-…)")?;
                let at_micros = mesh_proto::now_micros();
                let signature = ctx
                    .user
                    .sign(&mesh_proto::upgrade_cancel_signing_bytes(
                        &plan_id,
                        &ctx.user_id(),
                        at_micros,
                    ))
                    .to_vec();
                ctx.ipc(IpcRequest::UpgradeCancel {
                    plan_id,
                    by: ctx.user_id(),
                    at_micros,
                    signature,
                })
                .await?;
                println!("plan {plan_id} withdrawn — nodes it has not reached will refuse it");
                return Ok(0);
            }
            if let Some(selector) = on {
                let ctx = Ctx::new(&cli.global)?;
                let Some(version) = version else {
                    bail!("pass --to <version>: a fleet rollout names the release it rolls");
                };
                return run_fleet_upgrade(&ctx, selector, version, batch, move_pending, force, yes)
                    .await;
            }
            if rollback {
                let config = resolve_config(&cli.global, Overrides::default())?;
                let user = mesh_core::identity::load_cli_user(
                    &config.data_dir,
                    cli.global.user_identity.as_deref(),
                )
                .context("loading user identity")?;
                let report = meshware_node::upgrade::rollback(
                    &config,
                    &user,
                    meshware_node::upgrade::UpgradeOptions {
                        move_pending,
                        no_drain,
                        yes,
                        ..Default::default()
                    },
                )
                .await?;
                println!(
                    "meshnode {} restored at {}",
                    report.target,
                    report.exe.display()
                );
                return Ok(match report.daemon {
                    meshware_node::upgrade::DaemonOutcome::RolledBack(_)
                    | meshware_node::upgrade::DaemonOutcome::DrainedNotRestarted => 1,
                    _ => 0,
                });
            }
            if !self_ {
                bail!(
                    "pass --self to upgrade this binary, or --on <selector> --to <version> to roll a release across a set of nodes"
                );
            }
            let config = resolve_config(&cli.global, Overrides::default())?;
            let user = mesh_core::identity::load_cli_user(
                &config.data_dir,
                cli.global.user_identity.as_deref(),
            )
            .context("loading user identity")?;
            let report = meshware_node::upgrade::run(
                &config,
                &user,
                meshware_node::upgrade::UpgradeOptions {
                    check_only: check,
                    version,
                    move_pending,
                    no_drain,
                    yes,
                    exe: None,
                    from_file: from_file.map(std::path::PathBuf::from),
                    minisig: match &minisig {
                        Some(p) => Some(
                            std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
                        ),
                        None => None,
                    },
                    cosign: match &cosign {
                        Some(p) => Some(
                            std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
                        ),
                        None => None,
                    },
                    // An explicit flag wins; otherwise this node's own
                    // `[release]` section, which is what the daemon passes
                    // when it starts this process for a fleet rollout.
                    release_base: release_base.or_else(|| config.release.base.clone()),
                    // Explicit flags win — and that is also how the
                    // daemon hands down the keys this mesh trusts when it
                    // starts this process for a fleet rollout. Otherwise
                    // this node's own `[release]` section.
                    pubkeys: if release_key.is_empty() {
                        config.release.pubkey.clone().into_iter().collect()
                    } else {
                        release_key
                    },
                },
            )
            .await?;
            if check {
                println!(
                    "installed {} / latest {}{}",
                    report.current,
                    report.target,
                    if report.current == report.target {
                        " (up to date)"
                    } else {
                        ""
                    }
                );
                return Ok(0);
            }
            if let meshware_node::upgrade::DaemonOutcome::RolledBack(_) = report.daemon {
                // Said already, on stderr; the status is what a script reads.
                return Ok(1);
            }
            if report.installed {
                println!(
                    "meshnode {} installed at {}",
                    report.target,
                    report.exe.display()
                );
            }
            Ok(0)
        }
    }
}

/// Parse `--priority` and `--halt-on-fail`, and reject fan-out-only flags
/// on single-target runs so mistakes fail loudly instead of silently.
fn parse_dispatch_opts(
    priority: &str,
    halt_on_fail: Option<&str>,
    fanout: Option<u32>,
    max_parallel: Option<u32>,
) -> Result<(mesh_proto::JobPriority, Option<mesh_proto::HaltPolicy>)> {
    let priority = mesh_proto::JobPriority::parse(priority)
        .ok_or_else(|| anyhow::anyhow!("--priority takes low, normal, or high"))?;
    if fanout.is_none() && (halt_on_fail.is_some() || max_parallel.is_some()) {
        bail!("--max-parallel and --halt-on-fail require --fanout");
    }
    let halt = match halt_on_fail {
        None => None,
        Some(s) => Some(mesh_proto::HaltPolicy::parse(s).ok_or_else(|| {
            anyhow::anyhow!("--halt-on-fail takes a count or a percentage (e.g. 3 or 10%)")
        })?),
    };
    Ok((priority, halt))
}

/// `meshnode sources …` — Git sources (MASTERPLAN §8.1).
/// Wrap a submission in the dry-run verb.
///
/// The wrapper carries the request the caller would otherwise have sent,
/// untouched — which is what makes `--dry-run` a preview of the real
/// submission rather than of a second one assembled for previewing.
fn wrap_submission(inner: IpcRequest, dry_run: bool) -> IpcRequest {
    wrap_submission_with_key(inner, dry_run, false)
        .unwrap_or_else(|_| unreachable!("--explain-key is not set"))
}

/// [`wrap_submission`] plus `--explain-key` (C7 P1).
///
/// A second wrapper rather than a flag inside the first, because each
/// answers a different question and an operator who asked two should get
/// two answers rather than one merged one.
fn wrap_submission_with_key(
    inner: IpcRequest,
    dry_run: bool,
    explain_key: bool,
) -> Result<IpcRequest> {
    if explain_key {
        if dry_run {
            bail!("--explain-key dispatches nothing; it does not combine with --dry-run");
        }
        return Ok(IpcRequest::ExplainKey {
            request: Box::new(inner),
        });
    }
    Ok(if dry_run {
        IpcRequest::DryRun {
            request: Box::new(inner),
        }
    } else {
        inner
    })
}

/// Read a definition file, or stdin for `-`.
fn read_source(file: &str, what: &str) -> Result<String> {
    if file == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .with_context(|| format!("reading the {what} from stdin"))?;
        return Ok(buf);
    }
    std::fs::read_to_string(file).with_context(|| format!("reading {what} {file}"))
}

/// `meshnode facts …` (M18 §4.9).
async fn run_facts(global: &GlobalArgs, cmd: FactsCmd) -> Result<i32> {
    let ctx = Ctx::new(global)?;
    match cmd {
        FactsCmd::Show {
            subject,
            collector,
            output,
        } => {
            let resp = ctx
                .ipc(IpcRequest::Facts {
                    subject: subject.unwrap_or_default(),
                    collector,
                })
                .await?;
            let IpcResponse::Facts(rows) = resp else {
                return unexpected(resp);
            };
            format::print_facts(&rows, output == Output::Json)?;
            Ok(0)
        }
        FactsCmd::Query {
            expression,
            collector,
            count,
            output,
        } => {
            let resp = ctx
                .ipc(IpcRequest::FactsQuery {
                    expression: expression.clone(),
                    collector,
                })
                .await?;
            let IpcResponse::FactsMatched {
                expression,
                subjects,
                unknown,
            } = resp
            else {
                return unexpected(resp);
            };
            if count {
                println!("{}", subjects.len());
            } else {
                format::print_fact_matches(
                    &expression,
                    &subjects,
                    &unknown,
                    output == Output::Json,
                )?;
            }
            // Nothing matched is not an error — it is an answer, and a
            // script that greps for a kernel nobody runs wants 0 with an
            // empty list, not a failure.
            Ok(0)
        }
        FactsCmd::Diff {
            subject,
            collector,
            since,
            limit,
            output,
        } => {
            let since_micros = match since.as_deref() {
                Some(text) => Some(
                    mesh_proto::now_micros().saturating_sub(
                        humantime::parse_duration(text)
                            .map_err(|e| anyhow!("--since {text:?} is not a duration: {e}"))?
                            .as_micros()
                            .min(u128::from(u64::MAX)) as u64,
                    ),
                ),
                None => None,
            };
            let resp = ctx
                .ipc(IpcRequest::FactsDiff {
                    subject,
                    collector,
                    since_micros,
                    limit,
                })
                .await?;
            let IpcResponse::FactsDiff(rows) = resp else {
                return unexpected(resp);
            };
            format::print_fact_diff(&rows, output == Output::Json)?;
            Ok(0)
        }
    }
}

/// `meshnode collectors …` (M18 §4.5).
async fn run_collectors(global: &GlobalArgs, cmd: CollectorsCmd) -> Result<i32> {
    let ctx = Ctx::new(global)?;
    match cmd {
        CollectorsCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::FactCollectors).await?;
            let IpcResponse::FactCollectors(rows) = resp else {
                return unexpected(resp);
            };
            format::print_collectors(&rows, output == Output::Json)?;
            Ok(0)
        }
        CollectorsCmd::Add { file } => {
            let source = read_source(&file, "collector")?;
            // Parsed and signed client-side, like a schedule or a policy: a
            // malformed file fails before it reaches the daemon, and the
            // signature is only ever produced by the key that owns it.
            let collector = mesh_core::facts::parse_collector(&source, ctx.user_id())
                .map_err(|e| anyhow!("{e}"))?;
            let name = collector.name.clone();
            let signed = mesh_core::facts::sign_collector(&ctx.user, collector);
            match ctx
                .ipc(IpcRequest::FactCollectorPut {
                    collector: Box::new(signed),
                })
                .await?
            {
                IpcResponse::Ok => {
                    println!("collector '{name}' published");
                    println!("see what it reports:  meshnode facts --collector {name}");
                    println!("who runs it:          meshnode collectors ls");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        CollectorsCmd::Pause { name } => set_collector_paused(&ctx, &name, true).await,
        CollectorsCmd::Resume { name } => set_collector_paused(&ctx, &name, false).await,
        CollectorsCmd::Status { name } => collector_ring_status(&ctx, &name).await,
        CollectorsCmd::Widen { name, to, yes } => widen_collector(&ctx, &name, &to, yes).await,
    }
}

/// The canary readout (M24 §3.3).
///
/// A fact collector runs a script on every node it matches, unprompted —
/// nobody presses Run. So the interesting question about one is not "what
/// does it say" but "how many machines is it on, and for how long has that
/// been fine".
async fn collector_ring_status(ctx: &Ctx, name: &str) -> Result<i32> {
    let (row, matched) = collector_and_ring(ctx, name).await?;
    let now = mesh_proto::now_micros();
    println!("collector  {}", row.name);
    println!("runs       {}", row.run);
    println!("on         {}", row.on);
    println!(
        "ring       {} matching node{}, {} reporting",
        matched,
        if matched == 1 { "" } else { "s" },
        row.subjects
    );
    println!(
        "since      {} (last published)",
        format::since(now, row.created_at_micros)
    );
    if row.paused {
        println!("state      paused — it is on nodes, and running nothing");
    } else if !row.flapping_keys.is_empty() {
        println!(
            "state      FLAPPING on this node — {} changes every run, which is a collector \
             bug and not a fleet fact",
            row.flapping_keys.join(", ")
        );
    } else {
        println!("state      no flapping reported by this node");
    }
    println!();
    if let Some(wider) = widened(&row.on) {
        println!(
            "Widen the ring when you are satisfied:  meshnode collectors widen {} --to '{wider}'",
            row.name
        );
    } else {
        println!(
            "This runs everywhere its selector reaches. Narrow it to a ring first if you are \
             about to change what it does:"
        );
        println!(
            "  meshnode collectors widen {} --to '{}, tag=canary'",
            row.name, row.on
        );
    }
    Ok(0)
}

/// The obvious next ring: drop the trailing `tag=canary` clause.
fn widened(on: &str) -> Option<String> {
    if !on.contains(',') {
        return None;
    }
    let kept: Vec<&str> = on
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.eq_ignore_ascii_case("tag=canary"))
        .collect();
    (kept.len() < on.split(',').filter(|p| !p.trim().is_empty()).count())
        .then(|| kept.join(", "))
        .filter(|k| !k.is_empty())
}

/// Widening is an edit, so it is re-signed and re-published like every
/// other change to a collector — and it is confirmed, because it is a
/// deployment: it starts running a script on machines that were not
/// running it a second ago.
async fn widen_collector(ctx: &Ctx, name: &str, to: &str, yes: bool) -> Result<i32> {
    let (row, before) = collector_and_ring(ctx, name).await?;
    if row.built_in {
        bail!(
            "{name:?} is the built-in collector every node runs in process — its ring is \
             every node, always"
        );
    }
    let after = match ctx
        .ipc(IpcRequest::TargetPreview {
            selector: to.to_string(),
        })
        .await?
    {
        IpcResponse::Nodes(rows) => rows.len(),
        other => return unexpected(other),
    };
    if after == 0 {
        bail!("{to:?} matches no live node — refusing to publish a ring nobody is in");
    }
    println!(
        "{name}: {before} node{} now, {after} after this change.",
        if before == 1 { "" } else { "s" }
    );
    if after > before && !yes {
        println!(
            "{}",
            term::out().prose(&format!(
                "That starts `{}` on {} machine{} that are not running it.",
                row.run,
                after - before,
                if after - before == 1 { "" } else { "s" }
            ))
        );
        if !confirm_tty("widen the ring?") {
            bail!("aborted");
        }
    }
    let mut collector = mesh_core::facts::parse_collector(&row.source, ctx.user_id())
        .map_err(|e| anyhow!("{e}"))?;
    collector.on = to.trim().to_string();
    let signed = mesh_core::facts::sign_collector(&ctx.user, collector);
    match ctx
        .ipc(IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        })
        .await?
    {
        IpcResponse::Ok => {
            println!("collector '{name}' now runs on {to}");
            println!("watch it:  meshnode collectors status {name}");
            Ok(0)
        }
        other => unexpected(other),
    }
}

async fn collector_and_ring(
    ctx: &Ctx,
    name: &str,
) -> Result<(mesh_proto::FactCollectorRow, usize)> {
    let IpcResponse::FactCollectors(rows) = ctx.ipc(IpcRequest::FactCollectors).await? else {
        bail!("unexpected response to collectors");
    };
    let Some(row) = rows.into_iter().find(|r| r.name == name) else {
        bail!("no collector named {name:?} — `meshnode collectors ls` lists them");
    };
    let matched = match ctx
        .ipc(IpcRequest::TargetPreview {
            selector: row.on.clone(),
        })
        .await?
    {
        IpcResponse::Nodes(nodes) => nodes.len(),
        other => return unexpected(other).map(|_| unreachable!()),
    };
    Ok((row, matched))
}

/// Pausing is an **edit**: the record is re-signed and re-published, so
/// there is no verb that changes what a fleet reports without a signature
/// behind it. M8 established this for schedules; M18 reuses it, and that
/// reuse is why facts needed no tombstone op of its own.
async fn set_collector_paused(ctx: &Ctx, name: &str, paused: bool) -> Result<i32> {
    let resp = ctx.ipc(IpcRequest::FactCollectors).await?;
    let IpcResponse::FactCollectors(rows) = resp else {
        return unexpected(resp);
    };
    let Some(row) = rows.iter().find(|r| r.name == name) else {
        bail!("no collector named {name:?} — `meshnode collectors ls` lists them");
    };
    if row.built_in {
        bail!(
            "{name:?} is the built-in collector every node runs in process — it cannot \
             be paused, because a node with no facts cannot be selected, scheduled \
             against, or diagnosed"
        );
    }
    if row.paused == paused {
        println!(
            "collector '{name}' is already {}",
            if paused { "paused" } else { "armed" }
        );
        return Ok(0);
    }
    // The row carries the record's own YAML, so the edit round-trips
    // through the same parser that published it.
    let mut collector = mesh_core::facts::parse_collector(&row.source, ctx.user_id())
        .map_err(|e| anyhow!("{e}"))?;
    collector.paused = paused;
    let signed = mesh_core::facts::sign_collector(&ctx.user, collector);
    match ctx
        .ipc(IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        })
        .await?
    {
        IpcResponse::Ok => {
            println!(
                "collector '{name}' {}",
                if paused { "paused" } else { "resumed" }
            );
            Ok(0)
        }
        other => unexpected(other),
    }
}

/// `meshnode schedule …` (N1 P5).
async fn run_schedule(global: &GlobalArgs, cmd: ScheduleCmd) -> Result<i32> {
    let ctx = Ctx::new(global)?;
    match cmd {
        ScheduleCmd::Put { file, dry_run } => {
            let source = read_source(&file, "schedule")?;
            let parsed =
                mesh_core::schedules::parse(&source, ctx.user_id()).map_err(|e| anyhow!("{e}"))?;
            let name = parsed.name.clone();
            let signed = mesh_core::schedules::sign(&ctx.user, parsed);
            let resp = ctx
                .ipc(IpcRequest::SchedulePut {
                    schedule: Box::new(signed),
                    dry_run,
                })
                .await?;
            match resp {
                IpcResponse::ScheduleCheck(view) => {
                    format::print_schedule_check(&view)?;
                    Ok(0)
                }
                IpcResponse::Ok => {
                    println!("schedule '{name}' published");
                    println!("see when it fires:  meshnode schedule show {name}");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        ScheduleCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Schedules).await?;
            let IpcResponse::Schedules(rows) = resp else {
                return unexpected(resp);
            };
            format::print_schedules(&rows, output == Output::Json)?;
            Ok(0)
        }
        ScheduleCmd::Show { name, output } => {
            let resp = ctx
                .ipc(IpcRequest::ScheduleShow { name: name.clone() })
                .await?;
            let IpcResponse::ScheduleShow(row) = resp else {
                return unexpected(resp);
            };
            match output.as_str() {
                "toml" => {
                    print!(
                        "{}",
                        row.source
                            .clone()
                            .unwrap_or_else(|| "# no definition returned\n".into())
                    );
                    Ok(0)
                }
                "json" => {
                    println!("{}", serde_json::to_string_pretty(&*row)?);
                    Ok(0)
                }
                _ => {
                    format::print_schedule(&row)?;
                    Ok(0)
                }
            }
        }
        ScheduleCmd::Pause { name } => set_schedule_paused(&ctx, &name, true).await,
        ScheduleCmd::Resume { name } => set_schedule_paused(&ctx, &name, false).await,
        ScheduleCmd::Rm { name } => {
            let signature = mesh_core::schedules::sign_remove(&ctx.user, &name);
            let resp = ctx
                .ipc(IpcRequest::ScheduleRemove {
                    name: name.clone(),
                    by: ctx.user_id(),
                    signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("schedule '{name}' retired");
                    println!("jobs it already fired are untouched");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
    }
}

/// Pause or resume: fetch, flip, re-sign, publish — the discipline every
/// signed record in this CLI is edited by.
async fn set_schedule_paused(ctx: &Ctx, name: &str, paused: bool) -> Result<i32> {
    let resp = ctx
        .ipc(IpcRequest::ScheduleShow {
            name: name.to_string(),
        })
        .await?;
    let IpcResponse::ScheduleShow(row) = resp else {
        return unexpected(resp);
    };
    let source = row
        .source
        .clone()
        .ok_or_else(|| anyhow!("the daemon did not return the schedule's definition"))?;
    let mut sched =
        mesh_core::schedules::parse(&source, ctx.user_id()).map_err(|e| anyhow!("{e}"))?;
    if sched.paused == paused {
        println!(
            "schedule '{name}' is already {}",
            if paused { "paused" } else { "active" }
        );
        return Ok(0);
    }
    sched.paused = paused;
    let signed = mesh_core::schedules::sign(&ctx.user, sched);
    let resp = ctx
        .ipc(IpcRequest::SchedulePut {
            schedule: Box::new(signed),
            dry_run: false,
        })
        .await?;
    match resp {
        IpcResponse::Ok => {
            println!(
                "schedule '{name}' {}",
                if paused { "paused" } else { "resumed" }
            );
            Ok(0)
        }
        other => unexpected(other),
    }
}

/// `meshnode deployments …` (C2 §6).
/// `meshnode deployment validate` (Phase B, E4): parse and describe, with no
/// mesh — so it runs in CI, and a file's own mistakes are found before
/// anybody's node is asked about them.
fn deployment_validate(file: &str, output: Output) -> Result<i32> {
    let source = read_source(file, "deployment")?;
    let parsed = mesh_core::deployments::parse(&source, mesh_proto::UserId([0u8; 32]))
        .map_err(|e| anyhow!("{e}"))?;
    format::print_deployment_validate(
        &mesh_core::deployments::describe(&parsed),
        output == Output::Json,
    )?;
    Ok(0)
}

async fn run_deployment(global: &GlobalArgs, cmd: DeploymentCmd) -> Result<i32> {
    // Phase B, E4: three verbs about the *file*, answered before a mesh
    // context is built. A definition is valid or not wherever it is read,
    // and the CI runner checking one has no node to ask.
    let cmd = match cmd {
        DeploymentCmd::Validate { file, output } => return deployment_validate(&file, output),
        DeploymentCmd::Init { name, image } => {
            print!("{}", mesh_core::deployments::example(&name, &image));
            return Ok(0);
        }
        DeploymentCmd::Schema => {
            print!("{}", mesh_core::deployments::SCHEMA);
            return Ok(0);
        }
        other => other,
    };
    let ctx = Ctx::new(global)?;
    match cmd {
        DeploymentCmd::Put {
            file,
            dry_run,
            watch,
        } => {
            let deadline = watch.deadline()?;
            let source = read_source(&file, "deployment")?;
            let mut parsed = mesh_core::deployments::parse(&source, ctx.user_id())
                .map_err(|e| anyhow!("{e}"))?;
            let name = parsed.deployment.name.clone();
            let parsed_inline = parsed.inline.is_some();
            // `[deployment.container]`: publish the service this file
            // describes, then the deployment that names it. One file, one
            // command — the thing `nomad job run` does.
            //
            // **First**, because it is what settles `service_version`, and
            // the generation below is decided by comparing that against
            // what is published. Run the other way round it compares a
            // placeholder with a placeholder, sees no change, and the roll
            // silently does nothing.
            publish_inline(&ctx, &mut parsed, dry_run).await?;
            // Continue this deployment's generation sequence. Without it a
            // re-publish reuses the generation already running, the
            // reconciler sees nothing to replace, and the roll is a no-op
            // that reports success.
            set_generation(&ctx, &mut parsed).await?;
            // Phase B, E2: the file's knobs first, so a hold on promotion is
            // in place before the roll it holds. Only a whole file sends them.
            if !dry_run {
                let knobs = mesh_core::deployments::sign_knobs(&ctx.user, parsed.knobs.clone());
                match ctx
                    .ipc(IpcRequest::DeploymentKnobs {
                        knobs: Box::new(knobs),
                    })
                    .await?
                {
                    IpcResponse::Ok => {}
                    other => return unexpected(other),
                }
            }
            let signed = mesh_core::deployments::sign(&ctx.user, parsed.deployment);
            let policy = mesh_core::deployments::sign_policy(&ctx.user, parsed.policy);
            let resp = ctx
                .ipc(IpcRequest::DeploymentPut {
                    deployment: Box::new(signed),
                    policy: Box::new(policy),
                    dry_run,
                    inline_service: parsed_inline,
                })
                .await?;
            match resp {
                IpcResponse::DeploymentCheck(view) => {
                    format::print_deployment_check(&view)?;
                    Ok(0)
                }
                IpcResponse::Ok => {
                    println!("deployment '{name}' published");
                    format::print_deep_link(format::DeepLinkKind::Deployment, &name);
                    if watch.detach {
                        println!("watch it converge:  meshnode deployments status {name}");
                        return Ok(0);
                    }
                    follow_rollout(&ctx, &name, deadline).await
                }
                other => unexpected(other),
            }
        }
        DeploymentCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Deployments).await?;
            let IpcResponse::Deployments(rows) = resp else {
                return unexpected(resp);
            };
            format::print_deployments(&rows, output == Output::Json)?;
            Ok(0)
        }
        DeploymentCmd::Placement { output } => {
            let resp = ctx.ipc(IpcRequest::PlacementMap).await?;
            let IpcResponse::PlacementMap(rows) = resp else {
                return unexpected(resp);
            };
            format::print_placement(&rows, output == Output::Json)?;
            Ok(0)
        }
        DeploymentCmd::Show { name, output } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentShow { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentShow(row) = resp else {
                return unexpected(resp);
            };
            match output.as_str() {
                "toml" => {
                    print!(
                        "{}",
                        row.source
                            .clone()
                            .unwrap_or_else(|| "# no definition returned
"
                            .into())
                    );
                    Ok(0)
                }
                "json" => {
                    format::print_deployment(&row, true)?;
                    Ok(0)
                }
                _ => {
                    format::print_deployment(&row, false)?;
                    Ok(0)
                }
            }
        }
        DeploymentCmd::Status {
            name,
            output,
            stats,
        } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentStatus(view) = resp else {
                return unexpected(resp);
            };
            if !stats {
                format::print_deployment_status(&view, output == Output::Json)?;
                if output != Output::Json {
                    format::print_deep_link(format::DeepLinkKind::Deployment, &name);
                }
                return Ok(0);
            }
            // Phase B, E3: measured on each instance's own node, just now.
            let resp = ctx
                .ipc(IpcRequest::DeploymentInstanceStats { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentInstanceStats(rows) = resp else {
                return unexpected(resp);
            };
            if output == Output::Json {
                // One document, so a script reading this gets something a
                // JSON parser accepts rather than two concatenated objects.
                let doc = serde_json::json!({ "status": &*view, "stats": rows });
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                format::print_deployment_status(&view, false)?;
                format::print_instance_stats(&rows, false)?;
                format::print_deep_link(format::DeepLinkKind::Deployment, &name);
            }
            Ok(0)
        }
        DeploymentCmd::Rollout {
            name,
            service,
            watch,
        } => {
            let mesh_core::deployments::Parsed {
                deployment: mut dep,
                policy,
                ..
            } = fetch_deployment(&ctx, &name).await?;
            let (svc, ver) = service
                .split_once('@')
                .ok_or_else(|| anyhow!("a rollout target must pin a version, e.g. {service}@8"))?;
            let ver: u32 = ver.parse().context("service version is not a number")?;
            if svc != dep.service {
                return Err(anyhow!(
                    "deployment '{name}' runs service '{}', not '{svc}' — a rollout changes the version, not the service",
                    dep.service
                ));
            }
            if ver == dep.service_version {
                println!("deployment '{name}' is already at {}", dep.service_ref());
                return Ok(0);
            }
            dep.service_version = ver;
            dep.generation = dep.generation.saturating_add(1);
            let signed = mesh_core::deployments::sign(&ctx.user, dep);
            put_deployment(
                &ctx,
                signed,
                policy,
                &format!("rolling out to {svc}@{ver}"),
                &watch,
            )
            .await
        }
        // Phase B, E1: back to a whole published version, not only a service
        // version — the record and its side-car together.
        DeploymentCmd::Rollback {
            name,
            to: _,
            generation,
            tag,
            watch,
        } if generation.is_some() || tag.is_some() => {
            let history = fetch_history(&ctx, &name).await?;
            let version =
                mesh_core::deployments::find_version(&history, generation, tag.as_deref())
                    .map_err(|e| anyhow!("{e}"))?;
            let mut parsed = fetch_deployment(&ctx, &name).await?;
            let before = parsed.deployment.generation;
            mesh_core::deployments::revert_to(&mut parsed, &version).map_err(|e| anyhow!("{e}"))?;
            let what = format!(
                "going back to generation {}{} ({}){}",
                version.deployment.generation,
                tag.map(|t| format!(", tagged {t}")).unwrap_or_default(),
                version.deployment.service_ref(),
                if parsed.deployment.generation == before {
                    " — nothing that runs is replaced"
                } else {
                    ""
                }
            );
            let signed = mesh_core::deployments::sign(&ctx.user, parsed.deployment);
            put_deployment(&ctx, signed, parsed.policy, &what, &watch).await
        }
        DeploymentCmd::Rollback {
            name, to, watch, ..
        } => {
            let mesh_core::deployments::Parsed {
                deployment: mut dep,
                policy,
                ..
            } = fetch_deployment(&ctx, &name).await?;
            let target = match to {
                Some(v) => v,
                None => dep
                    .service_version
                    .checked_sub(1)
                    .filter(|v| *v >= 1)
                    .ok_or_else(|| {
                        anyhow!("deployment '{name}' is at version 1; name a target with --to")
                    })?,
            };
            if target == dep.service_version {
                println!("deployment '{name}' is already at {}", dep.service_ref());
                return Ok(0);
            }
            dep.service_version = target;
            dep.generation = dep.generation.saturating_add(1);
            let svc = dep.service.clone();
            let signed = mesh_core::deployments::sign(&ctx.user, dep);
            put_deployment(
                &ctx,
                signed,
                policy,
                &format!("rolling back to {svc}@{target}"),
                &watch,
            )
            .await
        }
        DeploymentCmd::Pause { name } => set_deployment_paused(&ctx, &name, true).await,
        DeploymentCmd::Resume { name } => set_deployment_paused(&ctx, &name, false).await,
        DeploymentCmd::Scale { name, count, watch } => {
            let mut parsed = fetch_deployment(&ctx, &name).await?;
            let previous =
                mesh_core::deployments::scale(&mut parsed, count).map_err(|e| anyhow!("{e}"))?;
            if previous == count {
                println!("deployment '{name}' already runs {count}");
                return Ok(0);
            }
            let signed = mesh_core::deployments::sign(&ctx.user, parsed.deployment);
            put_deployment(
                &ctx,
                signed,
                parsed.policy,
                &format!("scaling from {previous} to {count} — nothing that stays restarts"),
                &watch,
            )
            .await
        }
        DeploymentCmd::Restart {
            name,
            slot: None,
            watch,
        } => {
            let mut parsed = fetch_deployment(&ctx, &name).await?;
            mesh_core::deployments::restart(&mut parsed).map_err(|e| anyhow!("{e}"))?;
            let signed = mesh_core::deployments::sign(&ctx.user, parsed.deployment);
            put_deployment(
                &ctx,
                signed,
                parsed.policy,
                "restarting every instance, in batches under its rollout policy",
                &watch,
            )
            .await
        }
        DeploymentCmd::History { name, diff, output } => {
            let history = fetch_history(&ctx, &name).await?;
            format::print_deployment_history(&history, diff, output == Output::Json)?;
            if output != Output::Json {
                format::print_deep_link(format::DeepLinkKind::Deployment, &name);
            }
            Ok(0)
        }
        DeploymentCmd::Tag {
            name,
            tag,
            generation,
            description,
        } => {
            let history = fetch_history(&ctx, &name).await?;
            let version = mesh_core::deployments::find_version(&history, generation, None)
                .map_err(|e| anyhow!("{e}"))?;
            let at = version.deployment.generation;
            let signed = mesh_core::deployments::sign_tag(
                &ctx.user,
                mesh_proto::DeploymentTag {
                    deployment: name.clone(),
                    tag: tag.clone(),
                    version: Some(version),
                    description,
                    set_by: ctx.user_id(),
                    set_at_micros: 0,
                    signature: Vec::new(),
                },
            );
            let resp = ctx
                .ipc(IpcRequest::DeploymentTag {
                    tag: Box::new(signed),
                })
                .await?;
            let IpcResponse::Ok = resp else {
                return unexpected(resp);
            };
            println!("{}", term::out().prose(&format!("deployment '{name}': generation {at} is tagged {tag} — kept, and `deployment rollback {name} --tag {tag}` goes back to it")));
            Ok(0)
        }
        DeploymentCmd::Untag { name, tag } => {
            let signed = mesh_core::deployments::sign_tag(
                &ctx.user,
                mesh_proto::DeploymentTag {
                    deployment: name.clone(),
                    tag: tag.clone(),
                    version: None,
                    description: String::new(),
                    set_by: ctx.user_id(),
                    set_at_micros: 0,
                    signature: Vec::new(),
                },
            );
            let resp = ctx
                .ipc(IpcRequest::DeploymentTag {
                    tag: Box::new(signed),
                })
                .await?;
            let IpcResponse::Ok = resp else {
                return unexpected(resp);
            };
            println!("deployment '{name}': tag {tag} removed");
            Ok(0)
        }
        DeploymentCmd::Promote { name, watch } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentStatus(view) = resp else {
                return unexpected(resp);
            };
            let promotion =
                mesh_core::deployments::sign_promotion(&ctx.user, &name, view.generation);
            match ctx
                .ipc(IpcRequest::DeploymentPromote {
                    promotion: Box::new(promotion),
                })
                .await?
            {
                IpcResponse::Ok => {}
                IpcResponse::Error { message } => return Err(anyhow!("{message}")),
                other => return unexpected(other),
            }
            println!(
                "deployment '{name}': generation {} promoted — the roll continues",
                view.generation
            );
            if watch.detach {
                return Ok(0);
            }
            let deadline = watch.deadline()?;
            follow_rollout(&ctx, &name, deadline).await
        }
        DeploymentCmd::Instance { name, slot, output } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentInstance { name, slot })
                .await?;
            let IpcResponse::DeploymentInstance(detail) = resp else {
                return unexpected(resp);
            };
            format::print_deployment_instance(&detail, output == Output::Json)?;
            Ok(0)
        }
        // A terminal inside the instance: resolved here, and then exactly
        // the session `meshnode shell` opens — the far node checks the
        // grant, the reason and that the job is a container it runs.
        DeploymentCmd::Exec {
            name,
            slot,
            reason,
            command,
        } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentStatus(view) = resp else {
                return unexpected(resp);
            };
            let instance = mesh_core::deployments::pick_instance(&view, slot.as_deref())
                .map_err(|e| anyhow!("{e}"))?;
            let (node, job) = (instance.node.clone(), instance.job_id.short());
            sessions::instance(&ctx, &node, &job, &reason, command).await
        }
        DeploymentCmd::Logs { name, slot, follow } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentStatus(view) = resp else {
                return unexpected(resp);
            };
            let instance = mesh_core::deployments::pick_instance(&view, slot.as_deref())
                .map_err(|e| anyhow!("{e}"))?;
            // Which instance, on stderr — so a pipe gets only the output.
            eprintln!(
                "{} · slot {} on {}",
                instance.job_id.short(),
                instance.slot,
                instance.node
            );
            let mut stream = ctx
                .stream(IpcRequest::JobLogs {
                    job_id: instance.job_id,
                    follow,
                })
                .await?;
            consume_job_stream(&mut stream, false, false).await
        }
        // One instance: cancel its current incarnation, and the reconciler
        // replaces it on its next tick. Not a failure, so it spends none of
        // the restart budget.
        DeploymentCmd::Restart {
            name,
            slot: Some(slot),
            ..
        } => {
            let resp = ctx
                .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
                .await?;
            let IpcResponse::DeploymentStatus(view) = resp else {
                return unexpected(resp);
            };
            let instance = mesh_core::deployments::instance_in_slot(&view, &slot)
                .map_err(|e| anyhow!("{e}"))?;
            let (job, node) = (instance.job_id, instance.node.clone());
            match ctx.ipc(IpcRequest::JobCancel { job_id: job }).await? {
                IpcResponse::Error { message } => Err(anyhow!("{message}")),
                _ => {
                    println!(
                        "slot {slot} of '{name}': cancelled {} on {node}; the reconciler starts its replacement within a tick",
                        job.short()
                    );
                    Ok(0)
                }
            }
        }
        // Answered above, before a mesh context was built.
        DeploymentCmd::Validate { .. } | DeploymentCmd::Init { .. } | DeploymentCmd::Schema => {
            Ok(0)
        }
        DeploymentCmd::Rm { name } => {
            let signature = mesh_core::deployments::sign_remove(&ctx.user, &name);
            let resp = ctx
                .ipc(IpcRequest::DeploymentRemove {
                    name: name.clone(),
                    by: ctx.user_id(),
                    signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("deployment '{name}' deleted; its instances are being retired");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
    }
}

/// Fetch a deployment's current record by round-tripping its TOML — the
/// same trick `set_schedule_paused` uses for schedules.
/// Give a put the right generation, against what is published now.
///
/// The comparison itself — and the rule for which half of a stale record to
/// believe — lives in [`mesh_core::deployments::previous_from_row`], so
/// Director reaches the same number for the same file.
async fn set_generation(ctx: &Ctx, parsed: &mut mesh_core::deployments::Parsed) -> Result<()> {
    let name = parsed.deployment.name.clone();
    let prev = match ctx
        .ipc(IpcRequest::DeploymentShow { name: name.clone() })
        .await
    {
        Ok(IpcResponse::DeploymentShow(row)) => {
            mesh_core::deployments::previous_from_row(&row, ctx.user_id())
        }
        // No such deployment — a first publish, and generation 0.
        _ => None,
    };
    parsed.deployment.generation = mesh_core::deployments::generation_for(
        prev.as_ref().map(|(d, e)| (d, e.as_slice())),
        &parsed.deployment,
        &parsed.policy.instance_env,
    );
    Ok(())
}

/// Publish the service an inline `[deployment.container]` describes.
///
/// Nothing to do for a deployment that names a published service. For one
/// that carries an image, this is what makes the file self-contained: the
/// service is derived from the block, named after the deployment, and
/// versioned by content — so re-publishing an unchanged file rolls nothing
/// and changing the image rolls once.
async fn publish_inline(
    ctx: &Ctx,
    parsed: &mut mesh_core::deployments::Parsed,
    dry_run: bool,
) -> Result<()> {
    let Some(inline) = parsed.inline.clone() else {
        return Ok(());
    };
    // What is published under this name already, if anything. A missing
    // service is the ordinary first-publish case, not an error.
    let existing = match ctx
        .ipc(IpcRequest::ServiceShow {
            name: inline.service.clone(),
        })
        .await
    {
        Ok(IpcResponse::ServiceShow(row)) => Some(mesh_core::deployments::Existing::from(&row)),
        _ => None,
    };
    let outcome = mesh_core::deployments::resolve_inline(existing.as_ref(), parsed, ctx.user_id())
        .map_err(|e| anyhow!("{e}"))?;
    match outcome {
        mesh_core::deployments::InlineOutcome::Named
        | mesh_core::deployments::InlineOutcome::Reuse { .. } => Ok(()),
        mesh_core::deployments::InlineOutcome::Publish { version } => {
            // A dry run publishes nothing, so it reports what *would* be
            // created rather than creating it.
            if dry_run {
                println!(
                    "would publish service '{}' v{} — container {}",
                    version.name, version.version, version.entrypoint
                );
                return Ok(());
            }
            let signed = mesh_core::services::sign_service_version(&ctx.user, *version);
            let (name, v, image) = (
                signed.name.clone(),
                signed.version,
                signed.entrypoint.clone(),
            );
            // The head signature moves the service's "current version"
            // pointer, and is a separate signature for the same reason the
            // deployment's policy is: two statements, two proofs.
            let head_signature = mesh_core::services::sign_service_head(&ctx.user, &name, v);
            let resp = ctx
                .ipc(IpcRequest::ServiceCreate {
                    version: Box::new(signed),
                    head_signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok | IpcResponse::ServiceCreated { .. } => {
                    println!("container service '{name}' v{v} — {image}");
                    Ok(())
                }
                IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                    bail!(message)
                }
                other => bail!("unexpected response publishing the container: {other:?}"),
            }
        }
    }
}

/// A deployment's kept versions and its tags (Phase B, E1).
async fn fetch_history(ctx: &Ctx, name: &str) -> Result<mesh_proto::DeploymentHistoryView> {
    match ctx
        .ipc(IpcRequest::DeploymentHistory {
            name: name.to_string(),
        })
        .await?
    {
        IpcResponse::DeploymentHistory(view) => Ok(*view),
        IpcResponse::Error { message } => Err(anyhow!("{message}")),
        other => Err(anyhow!("unexpected response: {other:?}")),
    }
}

async fn fetch_deployment(ctx: &Ctx, name: &str) -> Result<mesh_core::deployments::Parsed> {
    let resp = ctx
        .ipc(IpcRequest::DeploymentShow {
            name: name.to_string(),
        })
        .await?;
    let IpcResponse::DeploymentShow(row) = resp else {
        return Err(anyhow!("no deployment named {name}"));
    };
    let source = row
        .source
        .clone()
        .ok_or_else(|| anyhow!("the daemon did not return the deployment's definition"))?;
    mesh_core::deployments::parse(&source, ctx.user_id()).map_err(|e| anyhow!("{e}"))
}

/// Re-publish a deployment, carrying its policy forward unchanged.
///
/// The policy is re-signed rather than re-sent verbatim because
/// `sign_policy` stamps `set_at_micros`, and a rollout genuinely is a new
/// statement of the same policy at a new time — an LWW record that never
/// refreshed its clock would lose to a stale one on a reconnecting node.
async fn put_deployment(
    ctx: &Ctx,
    signed: mesh_proto::Deployment,
    policy: mesh_proto::DeploymentPolicy,
    what: &str,
    watch: &RolloutWatch,
) -> Result<i32> {
    let name = signed.name.clone();
    let deadline = watch.deadline()?;
    let policy = mesh_core::deployments::sign_policy(&ctx.user, policy);
    let resp = ctx
        .ipc(IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        })
        .await?;
    match resp {
        IpcResponse::Ok => {
            println!("deployment '{name}': {what}");
            format::print_deep_link(format::DeepLinkKind::Deployment, &name);
            if watch.detach {
                println!("watch it converge:  meshnode deployments status {name}");
                return Ok(0);
            }
            follow_rollout(ctx, &name, deadline).await
        }
        other => unexpected(other),
    }
}

/// N1 P1 — stay with a rollout until it settles, redrawing as it goes.
///
/// **Polled, not streamed, deliberately.** A new streaming `IpcRequest`
/// variant would shift `IPCREQ_SESSION_OPEN_INDEX` — read off the wire
/// before decoding and pinned by a test — and would need a protocol minor
/// to buy an answer `DeploymentStatus` already gives completely. A
/// deployment's progress is a question about replicated state that every
/// node holds locally; asking it once a second over a pooled Unix socket
/// costs a map walk, and a subscription would have to be invalidated by
/// the same state changes we are polling for.
///
/// Ctrl-C stops the watching, never the rollout: the record is published
/// and signed before the first frame is drawn, and the elected reconciler
/// is a different process on a possibly different machine.
async fn follow_rollout(ctx: &Ctx, name: &str, deadline: std::time::Duration) -> Result<i32> {
    use format::RolloutOutcome;
    use std::io::{IsTerminal, Write};

    let live = std::io::stdout().is_terminal();
    let started = std::time::Instant::now();
    // The *progress* clock. Reset every time an instance reaches healthy,
    // so forty instances rolling one at a time never trip a deadline sized
    // for one of them.
    let mut last_progress = started;
    let mut best_healthy = 0u32;
    let mut last_line = String::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        ticker.tick().await;
        let resp = ctx
            .ipc(IpcRequest::DeploymentStatus {
                name: name.to_string(),
            })
            .await?;
        let IpcResponse::DeploymentStatus(view) = resp else {
            return unexpected(resp);
        };

        if view.healthy > best_healthy {
            best_healthy = view.healthy;
            last_progress = std::time::Instant::now();
        }

        // A `system` deployment whose selector matches nothing has nothing
        // to converge on, and "0/0 healthy ✓" would read as success. Say
        // what actually happened instead.
        if view.desired == 0 {
            println!(
                "{}",
                term::out().prose(&format!(
                    "! nothing to roll out — `{}` matches no node right now",
                    view.targets
                ))
            );
            if let Some(refusal) = &view.placement {
                for line in refusal.lines() {
                    println!("{}", crate::term::out().status(&format!("    · {line}")));
                }
            }
            println!("  The deployment stays published and will place itself when a node joins.");
            return Ok(1);
        }

        // A refusal nothing will fix on its own is a failure now, not in
        // ten minutes (N1 P2). Waiting out the deadline on a 48 GB service
        // in a mesh of 16 GB boxes teaches nobody anything — but waiting
        // out a node that is *momentarily* full is exactly right, and the
        // refusal itself knows which of the two it is.
        if let Some(refusal) = &view.placement
            && !refusal.transient()
        {
            print!("{}", format::render_rollout(&view, started.elapsed(), None));
            println!();
            println!(
                "{}",
                term::out().prose(&format!(
                    "! rollout failed — nothing in this mesh can take it as written\n  \
                 The deployment stays published. Widen `{}`, shrink what it asks for, \
                 or add a node.",
                    view.targets
                ))
            );
            std::io::stdout().flush()?;
            return Ok(1);
        }

        // Phase B, E2: a held canary set is where this watch's job ends —
        // the roll did what the file asked, and the next step is a person's.
        if view.awaiting_promotion {
            let frame = format::render_rollout(&view, started.elapsed(), None);
            if live {
                print!("\x1b[2J\x1b[H{frame}");
            } else {
                print!("{frame}");
            }
            println!();
            println!(
                "canaries healthy — holding for a person (auto_promote = false)\n  \
                 promote generation {} before its progress deadline stalls the roll:  meshnode deployment promote {name}",
                view.generation
            );
            std::io::stdout().flush()?;
            return Ok(0);
        }

        let outcome = if view.status == "paused" {
            Some(RolloutOutcome::Paused)
        } else if view.degraded_reason.is_some() {
            Some(RolloutOutcome::Degraded)
        } else if view.stalled_reason.is_some() {
            // The reconciler's own deadline, not this command's: it is the
            // one the deployment actually declared, it runs whether or not
            // anybody is watching, and it is what fires `auto_revert`.
            // `--timeout` only ever bounds the *watching*.
            Some(RolloutOutcome::DeadlineExceeded)
        } else if view.healthy >= view.desired && view.updating_from.is_none() && !view.canarying {
            Some(RolloutOutcome::Converged)
        } else if last_progress.elapsed() > deadline {
            Some(RolloutOutcome::StoppedWatching)
        } else {
            None
        };

        let elapsed = started.elapsed();
        let frame = format::render_rollout(&view, elapsed, Some(deadline));

        match outcome {
            Some(done) => {
                // The last frame stays in the scrollback either way — a
                // rollout that failed is read after the fact, and clearing
                // the table on the way out would take the evidence with it.
                if live {
                    print!("\x1b[2J\x1b[H{frame}");
                } else {
                    print!("{frame}");
                }
                println!();
                println!("{}", format::rollout_verdict(&view, done, elapsed));
                std::io::stdout().flush()?;
                return Ok(if done.ok() { 0 } else { 1 });
            }
            None if live => {
                print!(
                    "\x1b[2J\x1b[H{frame}\n  Ctrl-C stops watching, not the rollout · \
                     --detach skips this\n"
                );
                std::io::stdout().flush()?;
            }
            // Off a terminal — a pipe, CI, a log — escape codes are noise
            // and a frame a second is a wall of text. One line, and only
            // when a number actually moved.
            None => {
                let line = format!(
                    "  {} desired · {} placed · {} healthy · {} unhealthy",
                    view.desired,
                    view.running,
                    view.healthy,
                    format::rollout_unhealthy(&view)
                );
                if line != last_line {
                    println!("{line}");
                    std::io::stdout().flush()?;
                    last_line = line;
                }
            }
        }
    }
}

async fn set_deployment_paused(ctx: &Ctx, name: &str, paused: bool) -> Result<i32> {
    let mesh_core::deployments::Parsed {
        deployment: mut dep,
        policy,
        ..
    } = fetch_deployment(ctx, name).await?;
    if dep.paused == paused {
        println!(
            "deployment '{name}' is already {}",
            if paused { "paused" } else { "active" }
        );
        return Ok(0);
    }
    dep.paused = paused;
    let signed = mesh_core::deployments::sign(&ctx.user, dep);
    let policy = mesh_core::deployments::sign_policy(&ctx.user, policy);
    let resp = ctx
        .ipc(IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        })
        .await?;
    match resp {
        IpcResponse::Ok => {
            println!(
                "deployment '{name}' {}",
                if paused { "paused" } else { "resumed" }
            );
            Ok(0)
        }
        other => unexpected(other),
    }
}

/// `meshnode identity hardware …` (Y2/Y3).
///
/// Binding an identity to a token is a change to one file on one machine,
/// and every one of these verbs is about that file — but three of them need
/// the mesh (the admin count, the trusted roots, the attestation op), so
/// this builds a `Ctx` where it has to and not where it does not.
async fn run_identity_hardware(global: &GlobalArgs, cmd: HardwareCmd) -> Result<i32> {
    match cmd {
        HardwareCmd::Ls { socket, output } => {
            let socket = mesh_core::hardware::agent_socket(socket.as_deref())?;
            let keys = mesh_core::hardware::list(&socket)?;
            format::print_agent_keys(&socket, &keys, output == Output::Json)?;
            Ok(0)
        }

        HardwareCmd::Status { output } => {
            let path = identity_key_path(global)?;
            format::print_hardware_status(&path, output == Output::Json)
        }

        HardwareCmd::Use { key, socket, force } => {
            let path = identity_key_path(global)?;
            let socket = mesh_core::hardware::agent_socket(socket.as_deref())?;
            let keys = mesh_core::hardware::list(&socket)?;
            let wanted = key.trim().to_ascii_lowercase();
            let matches: Vec<_> = keys
                .iter()
                .filter(|k| {
                    k.user_id()
                        .to_display_string()
                        .to_ascii_lowercase()
                        .starts_with(&wanted)
                })
                .collect();
            let chosen = match matches.as_slice() {
                [one] => *one,
                [] => bail!(
                    "the agent at {} is holding no Ed25519 key starting {wanted:?}.\n  \
                     meshnode identity hardware ls    what it is holding",
                    socket.display()
                ),
                many => bail!(
                    "{wanted:?} matches {} keys in the agent; type more of it",
                    many.len()
                ),
            };

            // A key that is a file in ~/.ssh is the commonest wrong pick:
            // a desktop agent offers every one of them, and binding to one
            // says "hardware" while protecting nothing.
            if let mesh_core::hardware::KeyOrigin::File(file) = chosen.origin() {
                let note = format!(
                    "{} is {}, a key file on this computer — not a key on a security key. \
                     Binding to it would protect nothing.\n  meshnode identity hardware ls    \
                     which keys are which",
                    chosen.user_id().short(),
                    file.display()
                );
                if !force {
                    bail!(note);
                }
                eprintln!("warning (--force): {note}\n");
            }

            // What the file holds now, read from the file: that is what
            // binding replaces. A sealed key that is locked stops here
            // rather than being guessed at.
            let current = match mesh_core::hardware::identity_in_file(&path) {
                Ok(id) => id,
                Err(e) if force => {
                    eprintln!(
                        "note: could not read {} ({e}); it will be set aside, not overwritten",
                        path.display()
                    );
                    None
                }
                Err(e) => bail!(
                    "could not tell whose key {} holds: {e}\n  Unlock it first (`meshnode \
                     identity unlock`), or pass --force to set it aside unread.",
                    path.display()
                ),
            };
            let unreadable = current.is_none() && path.exists();

            // Landmines 6 and 7, decided before anything is written: a key
            // the mesh does not know would lock this machine out, and a key
            // alone on a token would make a lost token a lost mesh
            // (`mesh_core::hardware::check_bind`).
            let probe = current.unwrap_or_else(|| chosen.user_id());
            let old = match user_rows_for_bind(global, chosen).await {
                Ok(rows) => match mesh_core::hardware::check_bind(probe, chosen.user_id(), &rows) {
                    Ok(old) => old,
                    Err(refusal) if !force => bail!(bind_refusal(&refusal, &chosen.user_id())),
                    Err(refusal) => {
                        eprintln!(
                            "warning (--force): {}\n",
                            bind_refusal(&refusal, &chosen.user_id())
                        );
                        if probe == chosen.user_id() {
                            mesh_core::hardware::OldKey::Same
                        } else {
                            mesh_core::hardware::OldKey::Live
                        }
                    }
                },
                Err(e) if !force => bail!(
                    "could not ask the mesh whether {} is a member ({e}).\n  Binding a key \
                     the mesh does not know would lock this machine out, so this needs the \
                     mesh. Start the node, or pass --force: the key in {} is then set aside, \
                     never overwritten.",
                    chosen.user_id(),
                    path.display()
                ),
                Err(_) if probe == chosen.user_id() => mesh_core::hardware::OldKey::Same,
                Err(_) => mesh_core::hardware::OldKey::Live,
            };
            let old = if unreadable {
                mesh_core::hardware::OldKey::Live
            } else {
                old
            };

            let moved =
                mesh_core::hardware::bind_file(&path, &chosen.public_bytes(), old, current)?;
            println!("{} now signs with a key held by an agent.", path.display());
            println!("  identity  {}", chosen.user_id());
            println!("  agent     {}", socket.display());
            if !chosen.comment().is_empty() {
                println!("  key       {}", chosen.comment());
            }
            println!();
            match &moved {
                Some(to) => {
                    println!(
                        "Your previous key is still honoured by this mesh, so it was moved, not deleted:"
                    );
                    println!("  {}", to.display());
                    println!(
                        "Anyone who copies that file can act as it. Take it off this machine and \
                         keep it somewhere safe — it is how you get back in if the security key \
                         is lost."
                    );
                }
                None => println!(
                    "There is no private key on this disk any more — nothing to seal, nothing \
                     to back up, and nothing a backup could leak."
                ),
            }
            println!(
                "The mesh cannot yet tell this key from a file, though. Publish the chain:  \
                 meshnode identity hardware attest <cert>…"
            );
            Ok(0)
        }

        HardwareCmd::Attest { certs, kind, slot } => {
            let ctx = Ctx::new(global)?;
            let Some(kind) = mesh_proto::AttestKind::parse(&kind) else {
                bail!("unknown attestation kind (piv | openpgp | fido2)");
            };
            let mut chain = Vec::new();
            for path in &certs {
                let der =
                    std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                if der.starts_with(b"-----BEGIN") {
                    bail!(
                        "{} is PEM; this wants DER.\n  openssl x509 -in {} -outform der -out \
                         cert.der",
                        path.display(),
                        path.display()
                    );
                }
                chain.push(der);
            }
            let mut attestation = mesh_proto::HardwareAttestation {
                user_id: ctx.user_id(),
                kind,
                chain,
                slot,
                added_at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            if !attestation.well_formed() {
                bail!(
                    "that chain is not a shape the mesh will store: 1 to {} certificates, each \
                     under {} KiB",
                    mesh_proto::attest::MAX_CHAIN_CERTS,
                    mesh_proto::attest::MAX_CERT_BYTES / 1024
                );
            }
            attestation.signature = ctx.sign(&attestation.signing_bytes())?;
            let resp = ctx
                .ipc(IpcRequest::UserHardwareAttest {
                    attestation: Box::new(attestation),
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!(
                        "{}",
                        term::out().prose(
                            "Attested. `meshnode users` now shows this key as hardware-held."
                        )
                    );
                    Ok(0)
                }
                IpcResponse::Denied { message } => bail!(message),
                other => unexpected(other),
            }
        }

        HardwareCmd::Unattest { user, reason } => {
            let ctx = Ctx::new(global)?;
            let owner = match &user {
                None => ctx.user_id(),
                Some(name) => resolve_user_id(&ctx, name).await?,
            };
            let mut revocation = mesh_proto::AttestRevocation {
                user_id: owner,
                reason,
                revoked_by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            revocation.signature = ctx.sign(&revocation.signing_bytes())?;
            let resp = ctx
                .ipc(IpcRequest::UserHardwareRevoke {
                    revocation: Box::new(revocation),
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("Withdrawn. That key no longer reads as hardware-held.");
                    Ok(0)
                }
                IpcResponse::Denied { message } => bail!(message),
                other => unexpected(other),
            }
        }
    }
}

/// The mesh's user rows, for deciding whether a key may be bound
/// (Y2/Y3, `mesh_core::hardware::check_bind`).
///
/// Asked as this account's current key first, and then as the key being
/// bound: after `users rotate` the current key is revoked and the mesh
/// will not answer it, while the new one is a member. Only the second ask
/// can cost a touch, so it goes second.
async fn user_rows_for_bind(
    global: &GlobalArgs,
    chosen: &mesh_core::hardware::HardwareKey,
) -> Result<Vec<mesh_proto::UserRow>> {
    let mut ctx = Ctx::new(global)?;
    if let Ok(IpcResponse::Users(rows)) = ctx.ipc(IpcRequest::Users).await {
        return Ok(rows);
    }
    ctx.user = mesh_core::identity::Keypair::from_hardware(chosen.clone());
    match ctx.ipc(IpcRequest::Users).await? {
        IpcResponse::Users(rows) => Ok(rows),
        other => bail!("unexpected response to a user list: {other:?}"),
    }
}

/// A refused binding, as the steps that get past it.
///
/// The founder's steps differ from everybody else's because the founding
/// key can be neither rotated nor revoked: the way across is a second
/// admin, and the founding key stays the way back.
fn bind_refusal(refusal: &mesh_core::hardware::BindRefusal, chosen: &UserId) -> String {
    use mesh_core::hardware::BindRefusal;
    match refusal {
        BindRefusal::NotAMember {
            username,
            founder: true,
            ..
        } => format!(
            "{chosen} is not a member of this mesh, and the key this account uses now is — \
             binding it would lock this machine out.\n  Your key is the founding admin's, \
             which can never be rotated or revoked, so add the security key as a second \
             admin, then run this again:\n\n    meshnode users add {username}-key --key \
             {chosen} --role admin\n\n  The founding key is then moved aside, not \
             deleted. Keep it somewhere safe and off this machine: it is how you get back in \
             if the security key is lost."
        ),
        BindRefusal::NotAMember {
            username,
            admin: true,
            ..
        } => format!(
            "{chosen} is not a member of this mesh, and the key this account uses now is — \
             binding it would lock this machine out.\n  Carry your grants across to it first \
             (this retires the old key), then run this again:\n\n    meshnode users rotate \
             {username} --new-key {chosen}"
        ),
        BindRefusal::NotAMember { username, .. } => format!(
            "{chosen} is not a member of this mesh, and the key this account uses now is — \
             binding it would lock this machine out.\n  Ask an admin to replace your key \
             with it (in Director: Access › People › Replace key…), then run this \
             again:\n\n    meshnode users rotate {username} --new-key {chosen}"
        ),
        BindRefusal::SoleAdmin => "this would leave the mesh with one admin, whose key is on a \
             token nobody else can replace.\n  A lost token would then be a lost mesh, and \
             there is no recovery path for that.\n  Add a second admin first (`meshnode users \
             add … --role admin`), or pass --force if you genuinely mean it."
            .to_string(),
    }
}

/// Username, id, or id prefix → user id. One round trip, and the error
/// names what was typed rather than an internal id.
async fn resolve_user_id(ctx: &Ctx, arg: &str) -> Result<mesh_proto::UserId> {
    let IpcResponse::Users(rows) = ctx.ipc(IpcRequest::Users).await? else {
        bail!("could not read the mesh's users");
    };
    let needle = arg.trim().to_ascii_lowercase();
    let hit: Vec<&mesh_proto::UserRow> = rows
        .iter()
        .filter(|r| {
            r.username.to_ascii_lowercase() == needle
                || r.user_id
                    .to_display_string()
                    .to_ascii_lowercase()
                    .starts_with(&needle)
        })
        .collect();
    match hit.as_slice() {
        [one] => Ok(one.user_id),
        [] => bail!("no user matching {arg:?}"),
        many => bail!("{arg:?} matches {} users; type more of it", many.len()),
    }
}

async fn run_sources_cmd(ctx: &Ctx, cmd: SourcesCmd) -> Result<i32> {
    match cmd {
        SourcesCmd::AddGit {
            name,
            url,
            branch,
            interval,
            bump,
        } => {
            let interval = humantime::parse_duration(&interval)
                .context("invalid --interval (try 60s, 5m, 1h)")?;
            let source = mesh_core::gitsync::build_source(
                &ctx.user,
                name.clone(),
                url,
                branch,
                interval.as_secs().max(10),
                bump,
            );
            let resp = ctx.ipc(IpcRequest::SourceAdd { source }).await?;
            match resp {
                IpcResponse::Ok => {
                    println!("source '{name}' registered");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        SourcesCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Sources).await?;
            let IpcResponse::Sources(rows) = resp else {
                return unexpected(resp);
            };
            format::print_sources(&rows, output == Output::Json)?;
            Ok(0)
        }
        SourcesCmd::Rm { name, yes } => {
            if !yes {
                bail!("removing a source requires --yes");
            }
            let signature = ctx
                .user
                .sign(&mesh_proto::source_remove_signing_bytes(
                    &name,
                    &ctx.user_id(),
                ))
                .to_vec();
            let resp = ctx
                .ipc(IpcRequest::SourceRemove {
                    name: name.clone(),
                    by: ctx.user_id(),
                    signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("source '{name}' removed");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        SourcesCmd::Sync { name } => {
            let resp = ctx
                .ipc(IpcRequest::SourceSyncNow { name: name.clone() })
                .await?;
            match resp {
                IpcResponse::SourceSync {
                    commit,
                    bundle_id,
                    unchanged,
                    bumped,
                } => {
                    if unchanged {
                        println!("'{name}' unchanged at {}", &commit[..commit.len().min(12)]);
                    } else {
                        println!(
                            "'{name}' synced: {} → bundle {}",
                            &commit[..commit.len().min(12)],
                            bundle_id.short()
                        );
                        for (svc, version) in bumped {
                            println!("  bumped service '{svc}' to v{version}");
                        }
                    }
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
    }
}

/// `--fanout all|N` → the wire encoding (`Some(0)` = all matching nodes).
fn parse_fanout(arg: Option<&str>) -> Result<Option<u32>> {
    match arg {
        None => Ok(None),
        Some("all") => Ok(Some(0)),
        Some(n) => {
            let n: u32 = n
                .parse()
                .context("--fanout takes `all` or a positive count")?;
            if n == 0 {
                bail!("--fanout count must be positive (or `all`)");
            }
            Ok(Some(n))
        }
    }
}

/// Shared consumer for streaming job responses — both the single-target
/// framing and the fan-out framing (MASTERPLAN §9.3).
/// How a parallel run is rendered: `parallel` prints each item's output,
/// `map` prints where each item's output went.
struct ParallelView {
    progress: bool,
    /// `map`: outputs are object references, not bytes.
    objects: bool,
    /// `--out DIR`, remembered so the summary can say what will be fetched.
    out: Option<String>,
    /// Object references collected as items finished, in item order.
    fetched: Vec<(String, mesh_proto::ObjectRef)>,
    /// The run's id, so a completed `--out` fetch can tell the origin its
    /// results have been collected (R3 P3).
    job: Option<JobId>,
}

impl ParallelView {
    fn inline(progress: bool) -> ParallelView {
        ParallelView {
            progress,
            objects: false,
            out: None,
            fetched: Vec::new(),
            job: None,
        }
    }
}

/// `map`'s renderer: the same stream, plus the object references and the
/// reduce result. Returns the exit code and what was produced, so `--out`
/// can fetch it afterwards.
async fn consume_parallel_map(
    stream: &mut tokio::net::UnixStream,
    view: ParallelView,
) -> Result<(i32, Vec<(String, mesh_proto::ObjectRef)>, Option<JobId>)> {
    let mut view = view;
    let code = consume_parallel(stream, &mut view).await?;
    let job = view.job;
    Ok((code, std::mem::take(&mut view.fetched), job))
}

/// Render a parallel run: each item's output as it finishes, an optional
/// live per-node throughput line, and a summary. Output is grouped per item
/// rather than interleaved byte-by-byte, which is what GNU parallel does by
/// default and the only readable choice when 48 cores are writing at once.
async fn consume_parallel_stream(
    stream: &mut tokio::net::UnixStream,
    view: ParallelView,
) -> Result<i32> {
    let mut view = view;
    consume_parallel(stream, &mut view).await
}

async fn consume_parallel(
    stream: &mut tokio::net::UnixStream,
    view: &mut ParallelView,
) -> Result<i32> {
    let show_progress = view.progress;
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut total = 0u64;
    let mut this_job: Option<JobId> = None;
    loop {
        let resp: IpcResponse = match read_frame(stream).await {
            Ok(r) => r,
            Err(mesh_proto::ProtoError::Io(_)) => break,
            Err(e) => return Err(e.into()),
        };
        match resp {
            IpcResponse::ParallelStarted {
                job_id,
                total: n,
                workers,
                declined,
                estimate,
            } => {
                total = n;
                this_job = Some(job_id);
                view.job = Some(job_id);
                eprintln!(
                    "{}",
                    crate::term::err().status(&format!(
                        "→ {} — {n} item(s) on {} node(s): {}",
                        job_id.short(),
                        workers.len(),
                        workers.join(", ")
                    ))
                );
                // A node sitting the job out is stated, not swallowed: one
                // node quietly doing everything is the symptom operators
                // read as "the mesh is broken".
                for reason in &declined {
                    eprintln!("  ! {reason}");
                }
                // Roughly how long, before you decide whether to wait (R5
                // P4). A range and its sample count, never a single time: a
                // preview that says "17 minutes" is wrong often enough to be
                // untrusted forever, and one built from two runs should say
                // it was built from two runs.
                if let Some(e) = &estimate
                    && e.samples > 0
                {
                    eprintln!(
                        "  ~ {}–{} (p50 {}), from {} past run(s)",
                        format::duration_micros(e.p10_secs.saturating_mul(1_000_000)),
                        format::duration_micros(e.p90_secs.saturating_mul(1_000_000)),
                        format::duration_micros(e.p50_secs.saturating_mul(1_000_000)),
                        e.samples
                    );
                }
            }
            IpcResponse::ParallelItem {
                item,
                exit_code,
                stdout: item_stdout,
                stderr: item_stderr,
                node_name,
                output,
                ..
            } => {
                if !item_stdout.is_empty() {
                    stdout.write_all(item_stdout.as_bytes()).await?;
                    stdout.flush().await?;
                }
                if !item_stderr.is_empty() {
                    stderr.write_all(item_stderr.as_bytes()).await?;
                    stderr.flush().await?;
                }
                // Under `map` the output stayed on the node that produced
                // it. Printing where it went — rather than nothing — is
                // what makes that visible instead of merely true.
                if let Some(obj) = output {
                    if view.objects && view.out.is_none() {
                        // The *full* id, not the short one. This line is
                        // stdout, meant to be piped or pasted into
                        // `objects get` — and a prefix can only be
                        // resolved against objects held on this node,
                        // which by design is not where these are.
                        println!(
                            "{item}\t{}\t{}\t{node_name}",
                            obj.id.to_display_string(),
                            format::bytes(obj.size)
                        );
                    }
                    view.fetched.push((item.clone(), obj));
                }
                if exit_code != Some(0) {
                    eprintln!(
                        "{}",
                        crate::term::err().status(&format!(
                            "✗ {item} on {node_name}: {}",
                            exit_code
                                .map(|c| format!("exit {c}"))
                                .unwrap_or_else(|| "did not run".into())
                        ))
                    );
                }
            }
            IpcResponse::ParallelProgress {
                done,
                failed,
                chunk_items,
                speculated,
                per_node,
                ..
            } if show_progress => {
                let split = per_node
                    .iter()
                    .map(|n| format!("{} {}", n.name, n.items))
                    .collect::<Vec<_>>()
                    .join("  ");
                // A speculated chunk is mitigation, not a fault, so it is
                // named as re-runs rather than shown as an error count.
                let racing = if speculated > 0 {
                    format!(", {speculated} re-run")
                } else {
                    String::new()
                };
                eprintln!(
                    "  {done}/{total} done ({failed} failed{racing}), {chunk_items}/chunk — {split}"
                );
            }
            IpcResponse::ParallelProgress { .. } => {}
            IpcResponse::ParallelReduced {
                node_name,
                exit_code,
                stdout: reduce_stdout,
                stderr: reduce_stderr,
                inputs,
                ..
            } => {
                eprintln!("⇒ reduce over {inputs} output(s) on {node_name}");
                if !reduce_stdout.is_empty() {
                    stdout.write_all(reduce_stdout.as_bytes()).await?;
                    if !reduce_stdout.ends_with('\n') {
                        stdout.write_all(b"\n").await?;
                    }
                    stdout.flush().await?;
                }
                if !reduce_stderr.is_empty() {
                    stderr.write_all(reduce_stderr.as_bytes()).await?;
                    stderr.flush().await?;
                }
                if exit_code != Some(0) {
                    eprintln!(
                        "{}",
                        crate::term::err().status(&format!(
                            "✗ reduce: {}",
                            exit_code
                                .map(|c| format!("exit {c}"))
                                .unwrap_or_else(|| "did not run".into())
                        ))
                    );
                }
            }
            IpcResponse::ParallelDone {
                total,
                succeeded,
                failed,
                micros,
                speculated,
                per_node,
                halted,
            } => {
                let _ = std::io::stdout().flush();
                let secs = micros as f64 / 1e6;
                let untouched = total.saturating_sub(succeeded + failed);
                eprintln!(
                    "{}",
                    crate::term::err().status(&format!(
                        "← {succeeded}/{total} ok, {failed} failed{} in {secs:.1}s",
                        if untouched > 0 {
                            format!(", {untouched} untouched")
                        } else {
                            String::new()
                        }
                    ))
                );
                if let Some(why) = &halted {
                    // The breaker tripped. Said plainly, with the verb that
                    // picks the rest up once the script is fixed.
                    eprintln!("{}", crate::term::err().status(&format!("✗ {why}")));
                    if let Some(job) = this_job {
                        eprintln!("{}", term::err().prose(&format!("  fix it, then `meshnode {} --resume {}` re-runs what did not finish",
                            if view.objects { "map" } else { "parallel" },
                            job.short())));
                    }
                }
                if speculated > 0 {
                    eprintln!("    {speculated} chunk(s) re-run elsewhere to beat a straggler");
                }
                // The per-node split is the point of the whole feature: a
                // bigger node visibly did more, with nobody deciding it.
                for n in &per_node {
                    let share = if total > 0 {
                        n.items as f64 / total as f64 * 100.0
                    } else {
                        0.0
                    };
                    eprintln!(
                        "    {:<16} {:>7} items  {:>5.1}%  {:>7.1} items/s",
                        n.name, n.items, share, n.items_per_sec
                    );
                }
                return Ok(i32::from(failed > 0 || halted.is_some()));
            }
            IpcResponse::JobPreview(preview) => {
                format::print_job_preview(&preview)?;
                return Ok(0);
            }
            IpcResponse::Denied { message } => {
                eprintln!("permission denied: {message}");
                return Ok(1);
            }
            IpcResponse::Error { message } => {
                eprintln!("{}", term::error_line(&message.to_string()));
                return Ok(1);
            }
            other => {
                eprintln!("unexpected frame: {other:?}");
                return Ok(1);
            }
        }
    }
    Ok(1)
}

/// Read an object-fetch stream to its end, writing to `out` or stdout.
async fn drain_object(stream: &mut tokio::net::UnixStream, out: Option<&str>) -> Result<u64> {
    let mut file = match out {
        Some(path) => {
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            Some(std::fs::File::create(path)?)
        }
        None => None,
    };
    let mut stdout = tokio::io::stdout();
    // What actually arrived, against what the sender said it was sending.
    // An object is content-addressed and every chunk is verified on the way
    // in, but nothing checked that the *stream* delivered all of them — so a
    // fetch that ended early wrote a short file and reported success.
    let mut written: u64 = 0;
    loop {
        let resp: IpcResponse = match read_frame(stream).await {
            Ok(r) => r,
            Err(mesh_proto::ProtoError::Io(_)) => bail!("the object stream ended early"),
            Err(e) => return Err(e.into()),
        };
        match resp {
            IpcResponse::ObjectChunk { data } => {
                written = written.saturating_add(data.len() as u64);
                match &mut file {
                    Some(file) => file.write_all(&data)?,
                    None => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                }
            }
            IpcResponse::ObjectEnd { size } => {
                if written != size {
                    // Deliberately not a warning. A truncated result that
                    // looks like a whole one is the worst thing this path
                    // can hand back, and the caller writing it to a file has
                    // no other way to find out.
                    bail!(
                        "object transfer is incomplete: {written} bytes arrived, {size} expected. \
                         The file has NOT been written correctly — do not use it."
                    );
                }
                return Ok(size);
            }
            IpcResponse::Denied { message } => bail!("permission denied: {message}"),
            IpcResponse::Error { message } => bail!("{message}"),
            other => return Err(anyhow::anyhow!("unexpected frame: {other:?}")),
        }
    }
}

/// Resolve an object id or unique prefix against what this node holds.
/// Prefix matching is a convenience for typing, so it refuses an ambiguous
/// one rather than picking: guessing which object was meant is exactly the
/// wrong thing to do with content addresses.
async fn resolve_object_id(ctx: &Ctx, needle: &str) -> Result<mesh_proto::ObjectId> {
    if let Ok(id) = mesh_proto::ObjectId::parse(needle) {
        return Ok(id);
    }
    let resp = ctx.ipc(IpcRequest::Objects).await?;
    let IpcResponse::Objects(rows) = resp else {
        bail!("could not list objects to resolve {needle}");
    };
    let needle = needle.trim_start_matches("obj-");
    let matched: Vec<mesh_proto::ObjectId> = rows
        .iter()
        .map(|r| r.id)
        .filter(|id| id.to_display_string()["obj-".len()..].starts_with(needle))
        .collect();
    match matched.as_slice() {
        [one] => Ok(*one),
        // A prefix can only be matched against what is held here — the
        // directory is keyed by the whole content address, so there is no
        // "starts with" to ask the mesh. For an object on another node,
        // the full id is the way in, and `map` prints exactly that.
        [] => bail!(
            "no object held here matches {needle} — a short id only matches \
             objects on this node; paste the full obj-… id for one held elsewhere"
        ),
        many => bail!(
            "{} objects here match {needle}; use more of the id",
            many.len()
        ),
    }
}

/// Build, sign and register one service version, retrying the version
/// number the daemon names on a conflict.
///
/// One implementation, used by `service create` and by `add`. The signing,
/// the client-side version numbering and the conflict retry are subtle
/// enough that a second copy would drift, and the drift would show up as a
/// service that registers from one verb and not the other.
#[allow(clippy::too_many_arguments)]
async fn create_service_version(
    ctx: &Ctx,
    name: &str,
    runtime: mesh_proto::Runtime,
    bundle_id: mesh_proto::BundleId,
    working_dir: &str,
    entrypoint: &str,
    contract: Option<&str>,
    env: Vec<(String, String)>,
    description: &str,
    tags: Vec<String>,
) -> Result<u32> {
    // The version is built and signed client-side (MASTERPLAN §7): start
    // from what the mesh shows, and if a removed service's history makes the
    // daemon expect a higher number, retry with the number it names.
    let mut number = {
        let resp = ctx.ipc(IpcRequest::Services).await?;
        let IpcResponse::Services(rows) = resp else {
            bail!("could not list services to pick a version number");
        };
        rows.iter()
            .find(|r| r.name == name)
            .and_then(|r| r.versions.iter().max().copied())
            .unwrap_or(0)
            + 1
    };
    for _attempt in 0..2 {
        let version = services::sign_service_version(
            &ctx.user,
            ServiceVersion {
                name: name.to_string(),
                version: number,
                runtime,
                bundle_id,
                working_dir: working_dir.to_string(),
                entrypoint: entrypoint.to_string(),
                contract: contract.map(|c| c.to_string()),
                env: env.clone(),
                description: description.to_string(),
                tags: tags.clone(),
                created_at_micros: mesh_proto::now_micros(),
                created_by: ctx.user_id(),
                source: None,
                signature: Vec::new(),
            },
        );
        let head_signature = services::sign_service_head(&ctx.user, name, number);
        let resp = ipc::request(
            &ctx.socket(),
            &ctx.user,
            IpcRequest::ServiceCreate {
                version: Box::new(version),
                head_signature,
            },
        )
        .await?;
        match resp {
            IpcResponse::ServiceCreated { version, .. } => return Ok(version),
            IpcResponse::Error { message } if message.contains("version conflict") => {
                if let Some(expected) = parse_expected_version(&message) {
                    number = expected;
                    continue;
                }
                bail!("{message}");
            }
            IpcResponse::Denied { message } => bail!("permission denied: {message}"),
            IpcResponse::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        }
    }
    bail!("service create kept conflicting — is someone racing you? retry")
}

/// Read a script and its definitions file, if it has one.
///
/// One function because there is exactly one right answer to "what does
/// this script declare", and two callers who must not disagree about it:
/// `meshnode script inspect`, which shows it, and `meshnode add`, which
/// registers it. Director asks the same question of the same parser through
/// `script_inspect`.
fn read_script_spec(
    path: &std::path::Path,
) -> Result<(
    mesh_core::script_spec::ScriptSpec,
    mesh_core::script_spec::Provenance,
)> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let defs_path = std::path::PathBuf::from(mesh_core::script_spec::definitions_path(
        &path.to_string_lossy(),
    ));
    let definitions = match std::fs::read_to_string(&defs_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("cannot read {}: {e}", defs_path.display()),
    };
    mesh_core::script_spec::ScriptSpec::read_with_definitions(&source, definitions.as_deref())
        // Named, because "expected `,` at line 12" is only useful if you
        // know which of the two files it is talking about.
        .map_err(|e| anyhow::anyhow!("{}: {e}", defs_path.display()))
}

/// Show what a script says about itself, before anything is registered.
///
/// The point of the verb is that you can see the fields the Run screen will
/// show *without* registering, so "no arguments" is discovered here rather
/// than after the fact.
fn print_script_spec(
    path: &str,
    spec: &mesh_core::script_spec::ScriptSpec,
    from: mesh_core::script_spec::Provenance,
) {
    use mesh_core::script_spec::ArgType;
    let name = spec
        .name
        .clone()
        .unwrap_or_else(|| default_service_name(path));
    println!("name       {name}");
    if !spec.about.is_empty() {
        println!("about      {}", spec.about);
    }
    use mesh_core::script_spec::Provenance;
    println!(
        "declared   {}",
        match from {
            Provenance::Definitions => "yes — from the script's .definitions file",
            Provenance::Header => "yes — from the script's #meshware: header",
            Provenance::Inferred => "no — everything below was inferred by reading the script",
            Provenance::None => "no — the script says nothing about itself",
        }
    );
    if let Some(item) = &spec.item {
        println!("per item   each item fills in --{item}");
    }
    let needs: Vec<String> = [
        spec.cores.map(|c| format!("{c} core(s)")),
        spec.mem.clone().map(|m| format!("{m} memory")),
        spec.disk.clone().map(|d| format!("{d} disk")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !needs.is_empty() {
        println!("each item  {}", needs.join(", "));
    }
    if !spec.reads.is_empty() {
        println!("reads      {}", spec.reads.join(", "));
    }
    if !spec.writes.is_empty() {
        println!("writes     {}", spec.writes.join(", "));
    }
    if spec.args.is_empty() {
        println!("{}", term::out().prose("\nNo arguments. The Run screen will show no fields — add a\n`#meshware: arg NAME \"what it is\"` line for each one it takes."));
        return;
    }
    // Columns sized to what is in them. A truncated "one of libx265," reads
    // as a complete answer and is not one — the whole point of showing the
    // choices is that they are the choices.
    let rows: Vec<(String, String, String, &str, String)> = spec
        .args
        .iter()
        .map(|a| {
            let ty = match &a.ty {
                ArgType::Text => "text".to_string(),
                ArgType::Integer => "whole number".to_string(),
                ArgType::Number => "number".to_string(),
                ArgType::Bool => "yes / no".to_string(),
                ArgType::Choice(c) => format!("one of {}", c.join(", ")),
            };
            (
                format!("{}{}", a.name, if a.required { " *" } else { "" }),
                ty,
                a.default.clone().unwrap_or_else(|| "—".into()),
                if a.inferred { "inferred" } else { "declared" },
                a.description.clone(),
            )
        })
        .collect();
    let w = |f: fn(&(String, String, String, &str, String)) -> usize, head: usize| {
        rows.iter().map(f).max().unwrap_or(0).max(head)
    };
    let wn = w(|r| r.0.chars().count(), 8);
    let wt = w(|r| r.1.chars().count(), 4);
    let wd = w(|r| r.2.chars().count(), 7);
    println!(
        "\n{:<wn$}  {:<wt$}  {:<wd$}  {:<8}  DESCRIPTION",
        "ARGUMENT", "TYPE", "DEFAULT", "FROM"
    );
    for (name, ty, default, from, description) in &rows {
        println!("{name:<wn$}  {ty:<wt$}  {default:<wd$}  {from:<8}  {description}");
    }
    if spec.args.iter().any(|a| a.required) {
        println!("\n* required");
    }
    if spec.args.iter().any(|a| a.inferred) {
        println!("{}", term::out().prose("\nInferred rows are a guess from the flags this script reads. A\n`#meshware:` header replaces the guess with what you meant."));
    }
    for u in &spec.unknown {
        println!("\nnot understood by this version, and ignored: {u}");
    }
}

/// A service name from a path, when the script did not give itself one.
fn default_service_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "script".into())
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase()
}

/// `meshnode add FILE` — push, register and contract, in one verb (R2 P3).
///
/// This replaces the three-command dance that `scripts/library/install.sh`
/// existed to hide: `push`, then `service create --bundle … --entrypoint …
/// --contract …`, with the contract written by hand first. A person adding
/// their own script should not have to learn what a bundle is.
async fn add_script(
    global: &GlobalArgs,
    path: &str,
    name: Option<String>,
    entry: Option<String>,
    runtime: Option<String>,
) -> Result<i32> {
    let abs = std::fs::canonicalize(path).with_context(|| format!("cannot access {path}"))?;

    // What gets pushed, and what inside it runs.
    let (push_root, entrypoint) = if abs.is_file() {
        // A single script is a one-file bundle — plus its definitions file,
        // if it has one. Pushing its whole directory instead would sweep up
        // whatever else happens to live beside it, which for a script in a
        // home directory is everything. `ContentStore::ingest_path` draws
        // that line, so the push root here is simply the file.
        let file_name = abs
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("{path} has no filename"))?
            .to_string_lossy()
            .into_owned();
        (abs.clone(), file_name)
    } else {
        let entry = match entry {
            Some(e) => e,
            None => bail!(
                "{path} is a directory — say which file runs, with --entry NAME
                   (a single script is simpler: meshnode add ./thing.sh)"
            ),
        };
        if !abs.join(&entry).is_file() {
            bail!("{entry} is not a file inside {path}");
        }
        (abs.clone(), entry)
    };

    // The file that actually runs, wherever the push root turned out to be.
    let script = if push_root.is_file() {
        push_root.clone()
    } else {
        push_root.join(&entrypoint)
    };
    let (spec, from) = read_script_spec(&script)?;
    let service_name = name
        .or_else(|| spec.name.clone())
        .unwrap_or_else(|| default_service_name(&entrypoint));
    let runtime = runtime.unwrap_or_else(|| runtime_for(&entrypoint).to_string());

    let ctx = Ctx::new(global)?;
    let resp = ctx
        .ipc(IpcRequest::Push {
            path: push_root.to_string_lossy().into_owned(),
        })
        .await?;
    let IpcResponse::Pushed { bundle_id, .. } = resp else {
        return unexpected(resp);
    };

    let contract = spec.contract_json();
    let rt = mesh_proto::Runtime::parse(&runtime)
        .ok_or_else(|| anyhow::anyhow!("unknown runtime {runtime}"))?;
    let version = create_service_version(
        &ctx,
        &service_name,
        rt,
        bundle_id,
        "",
        &entrypoint,
        contract.as_deref(),
        Vec::new(),
        &spec.about,
        vec!["script".to_string()],
    )
    .await?;

    println!("added {service_name} (version {version}, {runtime})");
    use mesh_core::script_spec::Provenance;
    match (&contract, from) {
        (Some(_), Provenance::Definitions) => println!(
            "  {} argument(s), from {}",
            spec.args.len(),
            mesh_core::script_spec::definitions_path(&entrypoint)
        ),
        (Some(_), Provenance::Header) => println!(
            "  {} argument(s), from the script's #meshware: header",
            spec.args.len()
        ),
        (Some(_), _) => println!(
            "{}",
            term::out().prose(&format!(
                "  {} argument(s), inferred by reading the script — `meshnode script inspect {}` \
             to check them",
                spec.args.len(),
                path
            ))
        ),
        (None, _) => println!(
            "{}",
            term::out().prose(&format!(
                "  no arguments: the Run screen will show no fields. Write them in {} \
             (`meshnode script definitions {}`) and run this again.",
                mesh_core::script_spec::definitions_path(&entrypoint),
                path
            ))
        ),
    }
    if let Some(item) = &spec.item {
        println!("  per item: each item fills in --{item}");
    }
    println!("\n  meshnode service run {service_name}");
    Ok(0)
}

/// The runtime a file extension implies. `exec` is the honest answer for
/// anything else: it is a program, and the node runs it.
fn runtime_for(entrypoint: &str) -> &'static str {
    match std::path::Path::new(entrypoint)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("py") => "python",
        Some("yml") | Some("yaml") => "ansible",
        Some("sh") | Some("bash") | None => "bash",
        _ => "exec",
    }
}

async fn fetch_outputs(
    ctx: &Ctx,
    dir: &str,
    items: &[String],
    outputs: &[(String, mesh_proto::ObjectRef)],
) -> Result<()> {
    if outputs.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    // Name files by the item's position in the *input* list, not by the
    // order results happened to arrive in. Arrival order is a property of
    // how fast each node was, so it would rename every file on a re-run —
    // and `000003_` implying "the fourth item" while meaning "the fourth to
    // finish" is exactly the kind of near-miss that wastes an afternoon.
    let mut positions: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        positions.entry(item.as_str()).or_default().push_back(index);
    }
    let mut bytes = 0u64;
    // Names already handed out in this fetch, so two items with the same
    // basename land side by side instead of on top of each other.
    let mut taken: HashSet<String> = HashSet::new();
    for (item, obj) in outputs {
        let ordinal = positions
            .get_mut(item.as_str())
            .and_then(|q| q.pop_front())
            .unwrap_or(usize::MAX);
        let name = mesh_core::outputs::fetch_filename(ordinal, item, &mut taken);
        let path = std::path::Path::new(dir).join(&name);
        let mut stream = ctx
            .stream(IpcRequest::ObjectGet {
                id: obj.id,
                // The producer, straight from the item's own report — so
                // the common case never touches the directory at all.
                hints: vec![obj.holder],
            })
            .await?;
        bytes += drain_object(&mut stream, path.to_str()).await?;
    }
    eprintln!(
        "↓ {} output(s), {} into {dir}/",
        outputs.len(),
        format::bytes(bytes)
    );
    Ok(())
}

/// Tell the origin its results are safely here, so it stops holding them
/// past the ordinary object TTL (R3 P3).
///
/// Best effort and deliberately not fatal: the files are already written,
/// and failing a completed fetch because a bookkeeping call did not land
/// would be the wrong trade. Un-released results simply age out as they
/// always did.
async fn mark_collected(ctx: &Ctx, job: mesh_proto::JobId) {
    let _ = ctx.ipc(IpcRequest::ResultsCollected { job }).await;
}

/// What `--json` / `--outputs` / `--expect` add up to (M19 §5.2).
///
/// `--expect` implies capture: an assertion over rows that were never
/// captured would be an assertion over nothing, and silently passing is the
/// one thing a guard must never do.
fn capture_wanted(
    json: bool,
    outputs: Option<&str>,
    expect: Option<&str>,
) -> mesh_core::outputs::Wanted {
    match outputs {
        Some(list) => mesh_core::outputs::Wanted::keys(list.split(',')),
        None if json || expect.is_some() => mesh_core::outputs::Wanted::implicit(),
        None => mesh_core::outputs::Wanted::none(),
    }
}

/// Consume a submission's frame stream.
///
/// `announce` prints the "→ job on node" preamble and streams logs.
/// `rows` collects M19 result frames and prints one JSON array at the end
/// instead — the two are mutually exclusive on purpose, because a
/// machine-readable array with logs interleaved above it is neither.
async fn consume_job_stream(
    stream: &mut tokio::net::UnixStream,
    announce: bool,
    rows_as_json: bool,
) -> Result<i32> {
    consume_job_stream_raced(stream, announce, rows_as_json, false).await
}

/// `consume_job_stream`, told whether the submission was a race.
///
/// A race needs its own reading of the same frames. Every copy but the
/// winner is cancelled on purpose, and the ordinary fan-out summary counts
/// a cancelled target as a failure — so an entirely successful race printed
/// "2 succeeded, 1 failed" and, worse, exited non-zero. The frames are
/// right; what they *mean* depends on what was asked for, and only the
/// caller knows that.
async fn consume_job_stream_raced(
    stream: &mut tokio::net::UnixStream,
    announce: bool,
    rows_as_json: bool,
    racing: bool,
) -> Result<i32> {
    let mut report = RunReport::default();
    let view = runview::RunView::new(None, false);
    consume_job_stream_full(
        stream,
        None,
        announce,
        rows_as_json,
        racing,
        &mut report,
        view,
    )
    .await
}

/// What a run turned out to be, for whoever asked to be told afterwards
/// (`--notify`). Filled in as the frames go by; every field is optional
/// because a fan-out has no single node and a refused run has no job.
#[derive(Debug, Default)]
struct RunReport {
    job: Option<String>,
    node: Option<String>,
    state: Option<mesh_proto::JobState>,
    exit_code: Option<i32>,
}

/// `consume_job_stream_raced` with a frame the caller already read
/// (`--fallback local` peeks at the first one), a report to fill, and the
/// terminal to draw on.
///
/// Every line this prints goes through `view`, because on a terminal a
/// fan-out has a live board at the bottom of the screen and a line printed
/// past it would land in the middle of the board (`runview`).
async fn consume_job_stream_full(
    stream: &mut tokio::net::UnixStream,
    first: Option<IpcResponse>,
    announce: bool,
    rows_as_json: bool,
    racing: bool,
    report: &mut RunReport,
    view: runview::RunView,
) -> Result<i32> {
    let mut view = view;
    let mut first = first;
    let mut rows: Vec<mesh_proto::ResultRow> = Vec::new();
    let mut expect_failed = false;
    // What this run produced, for L2 P5's last line. One job or one fan-out
    // group — never both, and never a list: a link is to an object, and a
    // list of links is a table nobody asked for.
    let mut link: Option<(format::DeepLinkKind, String)> = None;
    loop {
        let resp: IpcResponse = match first.take() {
            Some(frame) => frame,
            None => match read_frame(stream).await {
                Ok(r) => r,
                Err(mesh_proto::ProtoError::Io(_)) => break,
                Err(e) => return Err(e.into()),
            },
        };
        match resp {
            IpcResponse::JobStarted {
                job_id,
                executor_name,
                ..
            } => {
                // Meaning first, id second. The id is what you paste into
                // `meshnode jobs show` an hour later; the sentence is what
                // you need now.
                view.job_started(&executor_name, &job_id.short(), announce);
                link = Some((format::DeepLinkKind::Job, job_id.to_display_string()));
                report.job = Some(job_id.short());
                report.node = Some(executor_name);
            }
            IpcResponse::LogChunk { stream, data } => {
                if !rows_as_json {
                    view.job_output(stream, &data)?;
                }
            }
            // M31 A2. On stderr and before the (absent) output, exactly as
            // the cache badge is: a script that pipes stdout still gets
            // clean bytes, and a person is told *why* there are none rather
            // than being left to conclude the job produced nothing.
            IpcResponse::LogsUnavailable { availability } => {
                if !rows_as_json {
                    view.say(&availability.message());
                }
            }
            // -- C7 the action cache (§11.2) --------------------------
            //
            // On stderr and *before* the output it stands in for, so a
            // script that pipes stdout sees exactly the bytes the original
            // run produced and a person sees the badge above them. "It
            // didn't run" and "it broke" are indistinguishable unless the
            // product says which.
            IpcResponse::CacheKey(explain) => {
                view.aside(|| format::print_key_explain(&explain, rows_as_json))?;
                return Ok(0);
            }
            IpcResponse::CacheHit(hit) => {
                if announce {
                    view.say(&format!(
                        "⚡ cached — {} ({} saved, produced by {})",
                        hit.key.short(),
                        format::bytes(hit.bytes_saved),
                        hit.produced_by.short()
                    ));
                }
            }
            IpcResponse::CacheMismatch { key, detail } => {
                // Loud, on stderr, and it does not fail the run: the job is
                // fine, the *entry* was wrong, and it has already been
                // dropped by the time this frame arrives.
                view.say(&format!(
                    "!! cache entry {} disagreed with this run: {detail}",
                    key.short()
                ));
                view.say("   the entry has been dropped and the mismatch audited");
            }
            // Context that did not stop the work — a borrowed slot, say
            // (C1 P5). On stderr, so it never joins a JSON array or a
            // captured stdout.
            IpcResponse::Notice { message } => view.note(&format!("· {message}")),
            // -- M19 results (§5.4) ------------------------------------
            IpcResponse::Result(row) => rows.push(*row),
            IpcResponse::Expected {
                expression,
                passed,
                failed,
            } => {
                expect_failed = !failed.is_empty();
                view.aside(|| format::print_expectation(&expression, passed, &failed));
            }
            IpcResponse::JobDone { state, exit_code } => {
                view.settle();
                let _ = std::io::stdout().flush();
                report.state = Some(state);
                report.exit_code = exit_code;
                if rows_as_json {
                    format::print_result_rows(&rows, true)?;
                } else if !rows.is_empty() {
                    format::print_result_rows(&rows, false)?;
                }
                if !rows_as_json {
                    // The last line, after the outcome: where to look at
                    // what just ran, in an address a colleague's machine
                    // understands (L2 P5).
                    let url = link.as_ref().map(|(kind, id)| format::deep_link(*kind, id));
                    view.job_done(state, exit_code, url.as_deref());
                }
                if expect_failed {
                    return Ok(1);
                }
                return Ok(exit_code.unwrap_or(if state.is_terminal() { 1 } else { 0 }));
            }
            IpcResponse::FanoutStarted { group, targets } => {
                // A fan-out is one object with many jobs in it, and the
                // group is the thing Director opens on (L2 P5).
                link = Some((format::DeepLinkKind::Fanout, group.to_display_string()));
                // No per-target job ids in the header any more: they are
                // `JobId::short()`, the job's first 10 base32 characters,
                // and those are its creation millisecond — so the jobs of
                // one fan-out, created together, printed the same id. The
                // group id is how they are asked for again.
                let kind = if racing {
                    runview::FanKind::Race
                } else {
                    runview::FanKind::Fanout
                };
                view.fan_started(
                    targets.into_iter().map(|t| t.node_name).collect(),
                    &group.short(),
                    kind,
                    !rows_as_json,
                );
            }
            IpcResponse::FanChunk {
                index,
                stream,
                data,
            } => {
                if !rows_as_json {
                    view.fan_chunk(index, stream, &data)?;
                }
            }
            IpcResponse::FanJobDone {
                index,
                state,
                exit_code,
            } => {
                // Recorded whether or not a person is watching the logs:
                // the summary is built from these, and has to be right.
                view.fan_job_done(index, state, exit_code)?;
            }
            // -- C4 gangs (§8) -----------------------------------------
            //
            // A gang reuses the fan-out's multiplexed framing once it is
            // running, so the three frames here are only about the part
            // that is new: the reservation.
            IpcResponse::GangWaiting {
                gang,
                held,
                size,
                round,
                behind,
            } => {
                if !rows_as_json {
                    // Said every round, because a `--follow` that prints
                    // nothing while it waits is indistinguishable from one
                    // that has hung — and this is the milestone where
                    // waiting is the normal case.
                    view.note(&match behind {
                        Some(senior) => format!(
                            "· {} holding {held}/{size} (round {round}) — behind {}",
                            gang.short(),
                            senior.short()
                        ),
                        None => format!(
                            "· {} holding {held}/{size} (round {round}) — waiting for room",
                            gang.short()
                        ),
                    });
                }
            }
            IpcResponse::GangStarted {
                gang,
                members,
                rounds,
            } => {
                view.fan_started(
                    members.iter().map(|m| m.node_name.clone()).collect(),
                    &gang.short(),
                    runview::FanKind::Gang { rounds },
                    !rows_as_json,
                );
                if !rows_as_json {
                    // Rank is what a gang's members call each other, so
                    // which machine holds which rank is worth saying.
                    let ranks: Vec<(String, String)> = members
                        .iter()
                        .map(|m| (m.node_name.clone(), m.job_id.short()))
                        .collect();
                    view.gang_ranks(&ranks);
                }
            }
            IpcResponse::GangDone {
                gang,
                outcome,
                succeeded,
                failed,
            } => {
                view.settle();
                let _ = std::io::stdout().flush();
                if rows_as_json {
                    format::print_result_rows(&rows, true)?;
                } else if !rows.is_empty() {
                    format::print_result_rows(&rows, false)?;
                }
                if !rows_as_json {
                    view.gang_summary(
                        &gang.short(),
                        outcome.as_str(),
                        outcome == mesh_proto::GangOutcome::Succeeded,
                        succeeded,
                        failed,
                    );
                }
                if expect_failed {
                    return Ok(1);
                }
                return Ok(i32::from(outcome != mesh_proto::GangOutcome::Succeeded));
            }
            IpcResponse::FanoutDone {
                succeeded,
                failed,
                skipped,
            } => {
                view.settle();
                let _ = std::io::stdout().flush();
                if rows_as_json {
                    format::print_result_rows(&rows, true)?;
                } else if !rows.is_empty() {
                    format::print_result_rows(&rows, false)?;
                }
                if !rows_as_json {
                    if racing {
                        // `failed` here is the losers: cancelled, or beaten
                        // and reported after the winner. Only whether
                        // *anybody* won decides the outcome.
                        view.race_summary(succeeded);
                    } else {
                        // L1 P5: `FanoutDone` counts successes and
                        // failures, and "failed" covers both a command that
                        // exited 2 and a node the mesh never heard from —
                        // which are different problems with different next
                        // steps. The per-target frames already told the
                        // view which was which; this count is the authority
                        // on how many. Nothing on the wire changed.
                        view.fan_summary(succeeded, failed, skipped);
                    }
                    if let Some((kind, id)) = &link {
                        view.link(&format::deep_link(*kind, id));
                    }
                }
                if expect_failed {
                    return Ok(1);
                }
                // A race succeeds when one copy does. Everything else was
                // cancelled to make that fast, and exiting non-zero for it
                // would make `--race` unusable in a script.
                if racing {
                    return Ok(i32::from(succeeded == 0));
                }
                return Ok(if failed == 0 && skipped == 0 { 0 } else { 1 });
            }
            IpcResponse::JobPreview(preview) => {
                view.aside(|| format::print_job_preview(&preview))?;
                return Ok(0);
            }
            IpcResponse::Denied { message } => {
                view.settle();
                bail!("permission denied: {message}")
            }
            IpcResponse::Error { message } => {
                view.settle();
                bail!("{message}")
            }
            other => {
                view.settle();
                bail!("unexpected stream frame: {other:?}")
            }
        }
    }
    // The stream ended without a terminal frame.
    //
    // Every ending above `return`s, so reaching this line means the daemon
    // stopped talking mid-job — it was drained, it crashed, or the socket
    // went away. It is never the job's doing: the job either finished or it
    // did not, and either way the daemon would have said so.
    //
    // This used to `Ok(0)`, which made "the node you were talking to went
    // away" look exactly like success — to a person and to a script. That
    // is the single confusion L1 P5 exists to end, so it is stated and it
    // exits non-zero.
    view.settle();
    let _ = std::io::stdout().flush();
    if !rows_as_json && !rows.is_empty() {
        format::print_result_rows(&rows, false)?;
    }
    eprintln!(
        "{}",
        crate::term::err().status(
            "← lost contact with the daemon before this finished. The mesh had a problem, not your \
         command — what ran, if anything, is unknown."
        )
    );
    eprintln!(
        "{}",
        term::err().prose("  `meshnode jobs` on any member says what the mesh saw.")
    );
    Ok(1)
}

// ---------------------------------------------------------------------------
// LAN pairing (MASTERPLAN §5.2)
// ---------------------------------------------------------------------------

/// Print a pairing code as digits, words and (usually) a QR.
///
/// One function for both sides of the exchange. The joiner's console and
/// the approver's screen are the two things a human is comparing, and a
/// code that rendered differently on them would read as an attack — so
/// there is one renderer here, over the one [`pairing::display`] in the
/// engine.
///
/// Everything goes to **stderr**, with the digits first. The digits are the
/// protocol (`mesh_core::pairing`); the words and the picture are ways of
/// reading them, and if the terminal mangles either, the comparison the
/// security argument rests on is still on screen.
fn print_pair_code(code: &str, style: QrStyle) {
    use std::io::IsTerminal as _;
    let shown = mesh_core::pairing::display(code);
    eprintln!("      confirmation code:  {}", shown.grouped);
    if let Some(words) = &shown.words {
        // The same six digits, said out loud. Two words carry the whole
        // code (`mesh_core::wordpair`), so reading these across a room is
        // exactly as strong as reading the digits — and much easier.
        eprintln!("                say it:  {words}");
    }
    let style = match style {
        // A QR in a log file or a CI transcript is forty lines of noise
        // nobody will ever point a camera at.
        QrStyle::Auto if std::io::stderr().is_terminal() => QrStyle::Blocks,
        QrStyle::Auto => QrStyle::Off,
        other => other,
    };
    let Some(qr) = shown.qr.filter(|_| style != QrStyle::Off) else {
        return;
    };
    eprintln!();
    let text = if style == QrStyle::Ascii {
        qr.to_ascii(4)
    } else {
        qr.to_half_blocks(4)
    };
    for line in text.lines() {
        eprintln!("  {line}");
    }
    eprintln!("  photograph the square to read this code off a headless machine");
}

/// Blocking y/N prompt on the terminal, off the async runtime.
async fn prompt_yes_no(question: String) -> bool {
    tokio::task::spawn_blocking(move || {
        eprint!("{question} [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    })
    .await
    .unwrap_or(false)
}

/// Joiner side, driven by a program rather than a person.
///
/// This is the half of `meshnode adopt` that runs on the machine being
/// adopted (see `adopt.rs`). It is the same pairing exchange the
/// interactive path runs — same commitment, same code, same mandatory
/// confirmation — with the two human touch points redirected:
///
/// - the code is **printed as one JSON line on stdout** instead of drawn
///   on a console nobody is looking at;
/// - the verdict is **read as one line from stdin** instead of typed.
///
/// Both ends of that redirection are the SSH channel the adopter already
/// authenticated, which is what makes the comparison sound: the adopter
/// reads this node's code back over a channel an attacker in the middle of
/// the *pairing* connection does not control, compares it with the code its
/// own daemon derived, and answers. A human eye and an SSH session are
/// doing the same job — carrying the code over a second, independent path —
/// and this one does not get tired at the tenth machine in the rack.
///
/// Frames (NDJSON, one object per line, stdout only):
/// `{"event":"listening",…}` · `{"event":"code",…}` ·
/// `{"event":"joined",…}` · `{"event":"error",…}`
async fn pair_join_scripted(
    config: &mesh_core::config::NodeConfig,
    wait: std::time::Duration,
) -> Result<mesh_core::membership::JoinReport> {
    use mesh_core::pairing;
    use std::io::Write as _;

    fn emit(value: serde_json::Value) {
        // Line-buffered and flushed per frame: the adopter is reading this
        // pipe live and a code that arrives when the process exits is a
        // code that arrives after the pairing timed out.
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }

    let (code_tx, mut code_rx) = tokio::sync::mpsc::channel(1);
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    let (listen_tx, listen_rx) = tokio::sync::oneshot::channel();
    let ux = pairing::JoinerUx {
        on_code: code_tx,
        decision: decision_rx,
        on_listening: Some(listen_tx),
        // A machine joining from a terminal is a machine; the person, if
        // there is one, enrols afterwards with `meshnode enrol`.
        person: None,
        on_announced: None,
    };
    let node_name = config.node_name.clone();
    tokio::spawn(async move {
        if let Ok(addr) = listen_rx.await {
            emit(serde_json::json!({
                "event": "listening",
                "port": addr.port(),
                "addr": addr.to_string(),
                "node_name": node_name,
            }));
        }
    });
    let answerer = tokio::spawn(async move {
        let Some(prompt) = code_rx.recv().await else {
            return;
        };
        emit(serde_json::json!({
            "event": "code",
            "code": prompt.code,
            // The word pair travels too. The adopter compares the digits —
            // they are the protocol — but a transcript somebody reads back
            // afterwards is far more useful with the words in it.
            "words": mesh_core::wordpair::phrase(&prompt.code),
            "mesh_name": prompt.mesh_name,
            "approver_name": prompt.approver_name,
            "approver_id": prompt.approver_id.to_display_string(),
        }));
        // One line on stdin: `yes` accepts, anything else declines. Read on
        // the blocking pool — stdin is a pipe here, and blocking a runtime
        // worker on it would stall the pairing protocol itself.
        let verdict = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return false;
            }
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
        .await
        .unwrap_or(false);
        let _ = decision_tx.send(verdict);
    });
    let report = pairing::pair_join(config, ux, Some(wait)).await;
    answerer.abort();
    match &report {
        Ok(r) => emit(serde_json::json!({
            "event": "joined",
            "mesh_name": r.mesh_name,
            "accepted_by": r.accepted_by.to_display_string(),
            "members": r.members,
        })),
        Err(e) => emit(serde_json::json!({ "event": "error", "message": e.to_string() })),
    }
    report.context("pairing failed")
}

/// Joiner side: announce, wait for a member, compare codes, join.
async fn pair_join_interactive(
    config: &mesh_core::config::NodeConfig,
    wait: std::time::Duration,
    qr_style: QrStyle,
) -> Result<mesh_core::membership::JoinReport> {
    use mesh_core::pairing;
    let (code_tx, mut code_rx) = tokio::sync::mpsc::channel(1);
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    let (listen_tx, listen_rx) = tokio::sync::oneshot::channel();
    let ux = pairing::JoinerUx {
        on_code: code_tx,
        decision: decision_rx,
        on_listening: Some(listen_tx),
        // A machine joining from a terminal is a machine; the person, if
        // there is one, enrols afterwards with `meshnode enrol`.
        person: None,
        on_announced: None,
    };
    eprintln!("{}", term::err().prose(&format!("waiting to pair as '{}' — approve from Director, or `meshnode invite approve` on any member…",
        config.node_name)));
    // On the LAN mDNS carries the address and nobody needs to see it. Across
    // the internet there is no mDNS, so print the exact command — splicing in
    // the operator's advertised host when they set one, because the bound
    // address is `0.0.0.0` (or loopback after the display rewrite) and neither
    // is dialable from anywhere else.
    let advertised_host = config
        .wan
        .advertise
        .first()
        .and_then(|a| a.rsplit_once(':'))
        .map(|(host, _)| host.to_string());
    tokio::spawn(async move {
        if let Ok(addr) = listen_rx.await {
            let shown = match advertised_host {
                Some(host) => format!("{host}:{}", addr.port()),
                None => format!("<this machine's address>:{}", addr.port()),
            };
            eprintln!("  from off this LAN:  meshnode invite approve --addr {shown}");
            eprintln!("                      (Director: Mesh → Pair by address)");
        }
    });
    // Drive the prompt concurrently with the protocol.
    let prompter = tokio::spawn(async move {
        if let Some(prompt) = code_rx.recv().await {
            eprintln!();
            // The id is the one the TLS handshake proved and the code is
            // bound to; the mesh name is the approver's word, printed clean
            // (SEC-9).
            eprintln!(
                "  Mesh '{}' wants to pair: member {} is asking.",
                mesh_proto::text::clean(&prompt.mesh_name, mesh_proto::text::MAX_NAME_CHARS),
                prompt.approver_id.short()
            );
            eprintln!();
            print_pair_code(&prompt.code, qr_style);
            eprintln!();
            let ok = prompt_yes_no("Does the member's screen show the same code?".into()).await;
            let _ = decision_tx.send(ok);
        }
    });
    let report = pairing::pair_join(config, ux, Some(wait)).await;
    prompter.abort();
    report.context("pairing failed")
}

/// Approver side: browse, pick a candidate, mint a single-use invite, run
/// the exchange through the daemon, confirm the code.
async fn invite_approve(
    ctx: &Ctx,
    node: Option<String>,
    addr: Option<String>,
    browse: std::time::Duration,
    tags: Vec<String>,
    qr_style: QrStyle,
) -> Result<i32> {
    use mesh_core::pairing;

    let addr = match addr {
        Some(a) => a,
        None => {
            eprintln!("browsing the LAN for nodes waiting to pair…");
            let candidates = pairing::browse(browse).await?;
            if candidates.is_empty() {
                bail!(
                    "no pairing candidates found — run `meshnode join --pair` on the new machine \
                     (same LAN), or pass --addr HOST:PORT directly"
                );
            }
            let candidate = match &node {
                Some(sel) => candidates
                    .iter()
                    .find(|c| {
                        c.node_name == *sel
                            || c.node_id
                                .is_some_and(|id| id.to_display_string().starts_with(sel.as_str()))
                    })
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no candidate matches {sel:?} (found: {})",
                            candidates
                                .iter()
                                .map(|c| c.node_name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                None if candidates.len() == 1 => candidates[0].clone(),
                None => {
                    eprintln!("several nodes are waiting to pair:");
                    for c in &candidates {
                        eprintln!(
                            "  {}  {}  {}{}{}",
                            c.node_name,
                            c.node_id.map(|i| i.short()).unwrap_or_default(),
                            c.addrs.first().map(|a| a.to_string()).unwrap_or_default(),
                            c.person
                                .as_deref()
                                .map(|p| format!("  ({p})"))
                                .unwrap_or_default(),
                            c.platform
                                .as_deref()
                                .map(|o| format!("  {o}"))
                                .unwrap_or_default(),
                        );
                    }
                    bail!("pick one with --node <name>");
                }
            };
            candidate
                .addrs
                .first()
                .ok_or_else(|| anyhow::anyhow!("candidate has no reachable address"))?
                .to_string()
        }
    };

    // Mint a single-use invite for provenance — built and signed here, like
    // `invite create` (the daemon never holds the admin key).
    let resp = ctx.ipc(IpcRequest::InviteBootstrap).await?;
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = resp
    else {
        return unexpected(resp);
    };
    let params = membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags,
        bootstrap: addrs,
    };
    let (token, public) = membership::build_invite(mesh_id, &ctx.user, &params, node_id)?;
    ctx.ipc(IpcRequest::InvitePublish { public }).await?;

    let mut stream = ctx
        .stream(IpcRequest::PairApprove {
            addr: addr.to_string(),
            token: token.encode(),
        })
        .await?;
    loop {
        let resp: IpcResponse = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(mesh_proto::ProtoError::Io(_)) => bail!("pairing stream ended unexpectedly"),
            Err(e) => return Err(e.into()),
        };
        match resp {
            IpcResponse::PairCode {
                session,
                code,
                joiner_name,
                joiner_id,
            } => {
                eprintln!();
                eprintln!(
                    "  meshnode on '{}' ({}) wants to join.",
                    mesh_proto::text::clean(&joiner_name, mesh_proto::text::MAX_NAME_CHARS),
                    joiner_id.short()
                );
                eprintln!();
                print_pair_code(&code, qr_style);
                eprintln!();
                let ok = prompt_yes_no("Does the joining machine show the same code?".into()).await;
                ctx.ipc(IpcRequest::PairDecide {
                    session,
                    accept: ok,
                })
                .await?;
                if !ok {
                    eprintln!("pairing declined.");
                }
            }
            IpcResponse::PairDone { node_id, node_name } => {
                println!(
                    "paired: '{}' ({}) is now a member",
                    mesh_proto::text::clean(&node_name, mesh_proto::text::MAX_NAME_CHARS),
                    node_id.short()
                );
                return Ok(0);
            }
            IpcResponse::Denied { message } => bail!("permission denied: {message}"),
            IpcResponse::Error { message } => bail!("{message}"),
            other => bail!("unexpected pairing frame: {other:?}"),
        }
    }
}

async fn run_service_cmd(ctx: &Ctx, cmd: ServiceCmd) -> Result<i32> {
    match cmd {
        ServiceCmd::Create {
            name,
            runtime,
            bundle,
            command,
            args,
            entrypoint,
            working_dir,
            contract,
            env,
            description,
            tags,
        } => {
            let runtime = Runtime::parse(&runtime).ok_or_else(|| {
                anyhow::anyhow!("unknown runtime {runtime:?} (bash|python|ansible|exec)")
            })?;
            let bundle_id = mesh_proto::BundleId::parse(&bundle).context("invalid bundle id")?;
            let contract = match contract {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading contract {path}"))?,
                ),
                None => None,
            };
            let mut env = parse_env(&env)?;
            // The command override rides the service's static environment,
            // like every other reserved requirement — so a deployment of
            // this service and an ad-hoc run of it ask for the same thing
            // in the same words.
            if (command.is_some() || !args.is_empty())
                && runtime.as_str() != mesh_proto::Runtime::Container.as_str()
            {
                bail!(
                    "--command and --arg apply to `--runtime container`; every other \
                     runtime takes its entry point from the bundle"
                );
            }
            env.extend(mesh_proto::command_env(command.as_deref(), &args));

            let number = create_service_version(
                ctx,
                &name,
                runtime,
                bundle_id,
                &working_dir,
                &entrypoint,
                contract.as_deref(),
                env,
                &description,
                tags,
            )
            .await?;
            println!("service '{name}' version {number} created");
            Ok(0)
        }
        ServiceCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Services).await?;
            let IpcResponse::Services(rows) = resp else {
                return unexpected(resp);
            };
            format::print_services(&rows, output == Output::Json)?;
            Ok(0)
        }
        ServiceCmd::Show { name, output } => {
            let resp = ctx
                .ipc(IpcRequest::ServiceShow { name: name.clone() })
                .await?;
            let IpcResponse::ServiceShow(row) = resp else {
                return unexpected(resp);
            };
            format::print_service(&row, output == Output::Json)?;
            if output != Output::Json {
                format::print_deep_link(format::DeepLinkKind::Script, &name);
            }
            Ok(0)
        }
        ServiceCmd::Run {
            name,
            on,
            set,
            timeout,
            fanout,
            priority,
            max_parallel,
            halt_on_fail,
            dry_run,
            json,
            outputs,
            expect,
            cacheable,
            cache_outputs,
            explain_key,
            gpus,
            vram_mb,
            device,
            gpu_share,
            gang,
            gang_placement,
            gang_restart,
        } => {
            let mut inputs = parse_env(&set)?;
            // The capture keys ride `inputs` here rather than the job
            // environment, because `RunService` has no `env` field and
            // growing the variant would break a mixed-version mesh — the
            // executor re-decodes this request out of the signed envelope
            // (M19 §5.2, amendment 8). The origin daemon reads them off and
            // strips them *before* the contract is validated, so a service
            // with `additionalProperties: false` never sees them.
            let want = capture_wanted(json, outputs.as_deref(), expect.as_deref());
            inputs.extend(want.to_env_with(expect.as_deref()));
            // `--cacheable` rides `inputs` for exactly the reason the
            // capture keys do: `RunService` has no `env` field, and growing
            // the variant is the one thing forbidden here — the executor
            // re-decodes this request out of the signed envelope, so an
            // older executor could not read a newer origin's submission at
            // all. The origin strips these before the contract is
            // validated, so a service with `additionalProperties: false`
            // never sees them.
            inputs.extend(cache_env(cacheable, cache_outputs.as_deref(), explain_key)?);
            // C5's device request rides `inputs` for the same reason and is
            // stripped at the same point. Without this a service could not
            // ask for an accelerator at all — only an ad-hoc command could,
            // which is backwards: a service is the shape that gets a
            // contract and a cache, and GPU work wants both.
            inputs.extend(parse_device_request(
                gpus,
                vram_mb,
                device.as_deref(),
                gpu_share,
            )?);
            // C4's gang keys ride `inputs` for the same reason and come
            // off at the same point — they start with the reserved prefix,
            // so `is_reserved_input` moves them into the job environment
            // before the contract is validated (R1 P4).
            //
            // Without this a gang could only ever be an ad-hoc command,
            // which is backwards for the same reason it was backwards for
            // GPUs: the things people actually run as a gang are published,
            // versioned and reviewed, and a pasted shell line is none of
            // those.
            inputs.extend(gang_env(
                gang,
                gang_placement.as_deref(),
                gang_restart,
                fanout.as_deref(),
            )?);
            let fanout = parse_fanout(fanout.as_deref())?;
            let (priority, halt_on_fail) =
                parse_dispatch_opts(&priority, halt_on_fail.as_deref(), fanout, max_parallel)?;
            let inner = IpcRequest::RunService {
                name,
                on,
                inputs,
                inventory: None,
                timeout_secs: timeout,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            };
            let mut stream = ctx
                .stream(wrap_submission_with_key(inner, dry_run, explain_key)?)
                .await?;
            consume_job_stream(&mut stream, !json, json).await
        }
        ServiceCmd::Rollback { name, version } => {
            let signature = services::sign_service_head(&ctx.user, &name, version);
            let resp = ctx
                .ipc(IpcRequest::ServiceRollback {
                    name: name.clone(),
                    version,
                    by: ctx.user_id(),
                    signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("service '{name}' now at version {version}");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
        ServiceCmd::Remove { name } => {
            let signature = services::sign_service_remove(&ctx.user, &name);
            let resp = ctx
                .ipc(IpcRequest::ServiceRemove {
                    name: name.clone(),
                    by: ctx.user_id(),
                    signature,
                })
                .await?;
            match resp {
                IpcResponse::Ok => {
                    println!("service '{name}' retired");
                    Ok(0)
                }
                other => unexpected(other),
            }
        }
    }
}

/// `meshnode runs …` — the run history and the live view (M10 P2).
/// Parse `--since` / `--until`.
///
/// Two forms, because two questions get asked: "the last 90 days" (`90d`,
/// `12h`, `30m`) and "since the incident" (`2026-01-31`, or with a time).
/// Refusing either would make somebody reach for `date -d` and a pipe.
fn parse_when(raw: &str, flag: &str) -> Result<u64> {
    let raw = raw.trim();
    if let Ok(dur) = humantime::parse_duration(raw) {
        return Ok(mesh_proto::now_micros().saturating_sub(dur.as_micros() as u64));
    }
    // `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS]`, read as UTC.
    let (date, time) = match raw.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (raw, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() == 3
        && let (Ok(y), Ok(m), Ok(d)) = (
            parts[0].parse::<i64>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
        )
    {
        let mut secs = days_from_civil(y, m, d) * 86_400;
        if let Some(t) = time {
            let hms: Vec<&str> = t.split(':').collect();
            secs += hms.first().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) * 3600;
            secs += hms.get(1).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) * 60;
            secs += hms.get(2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        }
        return Ok((secs.max(0) as u64).saturating_mul(1_000_000));
    }
    bail!("{flag} must be a duration like 90d or 12h, or a date like 2026-01-31, got {raw:?}")
}

/// Civil date to days since the Unix epoch (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

async fn run_history_cmd(ctx: &Ctx, cmd: HistoryCmd) -> Result<i32> {
    match cmd {
        HistoryCmd::Replay {
            job,
            from_end,
            limit,
            grep,
        } => {
            let job_id = resolve_job(ctx, &job).await?;
            let resp = ctx
                .ipc(IpcRequest::HistoryReplay {
                    job_id,
                    from_end,
                    limit,
                    search: grep.unwrap_or_default(),
                })
                .await?;
            let IpcResponse::PayloadWindow(window) = resp else {
                return unexpected(resp);
            };
            for line in &window.lines {
                println!("{line}");
            }
            if !window.status.is_empty() {
                // The destination's own words, on stderr so a pipe still
                // gets clean output — and a non-zero exit, because nothing
                // came back and a script must be able to tell.
                eprintln!("{}", window.status);
                return Ok(1);
            }
            if window.more_before {
                eprintln!(
                    "{}",
                    term::err().prose(&format!(
                        "— there is more above this window; page back with `--from-end {}`",
                        from_end + window.lines.len() as u64
                    ))
                );
            }
            Ok(0)
        }
        HistoryCmd::Jobs {
            state,
            node,
            since,
            until,
            grep,
            limit,
            offset,
            output,
        } => {
            let query = mesh_proto::HistoryQuery {
                state,
                user: None,
                node: match node {
                    Some(n) => Some(resolve_node(ctx, &n).await?),
                    None => None,
                },
                since_micros: since.map(|s| parse_when(&s, "--since")).transpose()?,
                until_micros: until.map(|s| parse_when(&s, "--until")).transpose()?,
                contains: grep,
                limit,
                offset,
            };
            let resp = ctx.ipc(IpcRequest::HistoryJobs(Box::new(query))).await?;
            let IpcResponse::HistoryJobs(page) = resp else {
                return unexpected(resp);
            };
            format::print_history_jobs(&page, output == Output::Json)?;
            Ok(0)
        }
        HistoryCmd::Stats { output } => {
            let resp = ctx.ipc(IpcRequest::HistoryStatus).await?;
            let IpcResponse::HistoryStatus(stats) = resp else {
                return unexpected(resp);
            };
            format::print_history_stats(&stats, output == Output::Json)?;
            Ok(0)
        }
        HistoryCmd::Results {
            group,
            failed,
            output,
        } => print_fanout_results(ctx, &group, failed, output).await,
        HistoryCmd::Diff {
            before,
            after,
            all,
            output,
        } => results_diff(ctx, &before, &after, all, output == Output::Json).await,
    }
}

/// Pull the expected version out of the daemon's conflict message
/// ("version conflict: expected N …").
fn parse_expected_version(message: &str) -> Option<u32> {
    message
        .split("expected ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn parse_role(s: &str) -> Result<Role> {
    Role::parse(s).ok_or_else(|| {
        anyhow::anyhow!("unknown role {s:?} (admin|operator|developer|auditor|viewer)")
    })
}

fn parse_expiry(expires: Option<String>) -> Result<Option<u64>> {
    match expires {
        None => Ok(None),
        Some(s) => {
            let dur = humantime::parse_duration(&s).context("invalid --expires (try 30d, 12h)")?;
            Ok(Some(mesh_proto::now_micros().saturating_add(
                u64::try_from(dur.as_micros()).unwrap_or(u64::MAX),
            )))
        }
    }
}

/// Build and sign a grant for a user.
fn build_grant(
    ctx: &Ctx,
    user_id: UserId,
    role: Role,
    nodes: String,
    services_glob: String,
    expires_at_micros: Option<u64>,
) -> Result<Grant> {
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id,
        role,
        nodes,
        services: services_glob,
        expires_at_micros,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: ctx.user_id(),
        signature: Vec::new(),
    };
    grant.signature = ctx.sign(&grant.signing_bytes())?;
    Ok(grant)
}

/// The device-code login (E1-P5).
///
/// The CLI prints a code and waits; **the daemon does every network round
/// trip and the verification**, because the node is the thing holding the
/// admin-signed provider record, the key cache and the clock. A client that
/// fetched its own token would be asking the mesh to believe a string it
/// was handed.
async fn run_login(ctx: &Ctx, provider: &str, wait: &str) -> Result<i32> {
    let budget = humantime::parse_duration(wait).context("invalid --wait (try 5m)")?;
    // A `--wait` too long to add to a clock is "as long as it takes".
    let now = std::time::Instant::now();
    let deadline = now
        .checked_add(budget)
        .unwrap_or_else(|| now + std::time::Duration::from_secs(100 * 365 * 86_400));

    let started = ctx
        .ipc(IpcRequest::OidcLoginBegin {
            provider: provider.to_string(),
        })
        .await?;
    let IpcResponse::OidcLoginStarted {
        user_code,
        verification_uri,
        verification_uri_complete,
        device_code,
        mut interval,
        expires_in,
    } = started
    else {
        return unexpected(started);
    };

    println!("Sign in to {provider} to continue.");
    println!();
    if verification_uri_complete.is_empty() {
        println!("  1. open  {verification_uri}");
        println!("  2. enter {user_code}");
    } else {
        println!("  open  {verification_uri_complete}");
        println!("  code  {user_code}   (if you are asked for it)");
    }
    println!();
    println!(
        "Waiting up to {}…",
        humantime::format_duration(budget.min(std::time::Duration::from_secs(expires_in)))
    );

    loop {
        if std::time::Instant::now() >= deadline {
            bail!("gave up waiting after {wait} — run `meshnode login {provider}` again");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
        let resp = ctx
            .ipc(IpcRequest::OidcLoginPoll {
                provider: provider.to_string(),
                device_code: device_code.clone(),
                interval,
            })
            .await?;
        match resp {
            IpcResponse::OidcLoginPending { interval: next } => {
                interval = next;
            }
            IpcResponse::OidcLoginVerified {
                subject,
                email,
                display_name,
                groups,
                suggested_role,
            } => {
                println!();
                println!("Verified by {provider}.");
                if !display_name.is_empty() {
                    println!("  you        {display_name}");
                }
                if !email.is_empty() {
                    println!("  email      {email}");
                }
                println!("  subject    {subject}");
                if !groups.is_empty() {
                    println!("  groups     {}", groups.join(", "));
                }
                if let Some(r) = suggested_role {
                    println!("  suggests   {r}");
                }
                println!();
                println!("This proves who you are; it does not grant anything. Ask to join:");
                println!();
                println!("  meshnode enrol --as <name>");
                println!();
                println!("An admin approving your request will now see the identity above.");
                return Ok(0);
            }
            other => return unexpected(other),
        }
    }
}

async fn run_oidc_cmd(ctx: &Ctx, cmd: OidcCmd) -> Result<i32> {
    let OidcCmd::Providers { cmd } = cmd;
    match cmd.unwrap_or(OidcProvidersCmd::Ls {
        output: Output::Table,
    }) {
        OidcProvidersCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::OidcProviders).await?;
            let IpcResponse::OidcProviders(rows) = resp else {
                return unexpected(resp);
            };
            format::print_oidc_providers(&rows, output == Output::Json)?;
            Ok(0)
        }
        OidcProvidersCmd::Put {
            name,
            issuer,
            client_id,
            jwks_uri,
            device_endpoint,
            token_endpoint,
            scopes,
            groups_claim,
            group_roles,
        } => {
            let mut parsed = Vec::new();
            for gr in &group_roles {
                let (g, r) = gr
                    .split_once('=')
                    .with_context(|| format!("--group-role wants group=role, got {gr:?}"))?;
                parsed.push(mesh_proto::GroupRole {
                    group: g.to_string(),
                    role: parse_role(r)?,
                });
            }
            let mut provider = mesh_proto::OidcProvider {
                name: name.clone(),
                issuer,
                client_id,
                jwks_uri,
                device_authorization_endpoint: device_endpoint,
                token_endpoint,
                scopes,
                groups_claim,
                group_roles: parsed,
                configured_by: ctx.user_id(),
                configured_at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            provider.signature = ctx.sign(&provider.signing_bytes())?;
            ctx.ipc(IpcRequest::OidcProviderPut { provider }).await?;
            println!("identity provider '{name}' configured");
            println!();
            println!("People can now sign in with:  meshnode login {name}");
            Ok(0)
        }
        OidcProvidersCmd::Rm { name } => {
            let by = ctx.user_id();
            let at_micros = mesh_proto::now_micros();
            let signature = ctx
                .user
                .sign(&mesh_proto::oidc_provider_remove_signing_bytes(
                    &name, &by, at_micros,
                ))
                .to_vec();
            ctx.ipc(IpcRequest::OidcProviderRemove {
                name: name.clone(),
                by,
                at_micros,
                signature,
            })
            .await?;
            println!("identity provider '{name}' removed");
            Ok(0)
        }
    }
}

/// Machine principals (E1-P4).
///
/// `create` is three signed requests in a deliberate order — `UserAdd`,
/// then `ServiceAccountAdd`, then `GrantAdd` — rather than one compound
/// call. Each artifact is independently signed and verified at merge on
/// every node, which is the property the whole governance model rests on,
/// and each lands in the audit log as the separate fact it is. A compound
/// request would need a second copy of three authorization checks and
/// would report three facts as one row.
///
/// The order matters: the marker goes in **before** the grant, so the
/// daemon's admin refusal sees a machine when it evaluates the grant.
async fn run_service_accounts_cmd(ctx: &Ctx, cmd: ServiceAccountsCmd) -> Result<i32> {
    match cmd {
        ServiceAccountsCmd::Create {
            name,
            role,
            nodes,
            services: services_glob,
            expires_in,
            never_expires,
            description,
            out,
        } => {
            let role = parse_role(&role)?;
            if role == mesh_proto::Role::Admin {
                bail!(
                    "a service account may not hold the admin role.\n\n\
                     An admin key can mint users and grants, so a leaked pipeline credential \
                     holding it is permanent mesh compromise — and the reason to have a machine \
                     principal at all is that it is narrower than a person's.\n\n\
                     Pick operator, developer, auditor or viewer. If a machine genuinely needs \
                     to administer this mesh, use a person's key and own that decision."
                );
            }
            if expires_in.is_none() && !never_expires {
                bail!(
                    "say how long this credential may act: --expires-in 90d\n\n\
                     A machine credential with no clock on it is the long-lived bearer token \
                     this command exists to replace. If you really want one, --never-expires \
                     says so out loud and is recorded in the audit log."
                );
            }
            let expires_at_micros = parse_expiry(expires_in)?;

            // Minted here, on the admin's machine, and never anywhere else.
            // The daemon never sees it; the mesh replicates only the public
            // half, exactly as it does for a person.
            let kp = mesh_core::identity::Keypair::generate()
                .context("could not generate a keypair for the service account")?;
            let user_id = kp.user_id();

            let mut record = UserRecord {
                user_id,
                username: name.clone(),
                created_at_micros: mesh_proto::now_micros(),
                added_by: ctx.user_id(),
                signature: Vec::new(),
            };
            record.signature = ctx.sign(&record.signing_bytes())?;
            ctx.ipc(IpcRequest::UserAdd { record }).await?;

            let mut account = mesh_proto::ServiceAccount {
                user_id,
                name: name.clone(),
                description,
                created_at_micros: mesh_proto::now_micros(),
                created_by: ctx.user_id(),
                expires_at_micros,
                signature: Vec::new(),
            };
            account.signature = ctx.sign(&account.signing_bytes())?;
            ctx.ipc(IpcRequest::ServiceAccountAdd { account }).await?;

            let grant = build_grant(
                ctx,
                user_id,
                role,
                nodes.clone(),
                services_glob,
                // The *grant* rides the account's clock too, so `grants ls`
                // does not show a live grant on a principal that is out of
                // time. Belt and braces: `roles_at` already refuses.
                expires_at_micros,
            )?;
            ctx.ipc(IpcRequest::GrantAdd { grant }).await?;

            // The one moment this key exists outside the process that made
            // it. There is no recovery path and that is deliberate: a
            // credential meshware could re-print is a credential meshware
            // stores.
            //
            // **Always a file, never stdout.** A private key echoed to a
            // terminal is in scrollback, in the shell history of whatever
            // piped it, and in the scroll buffer of whoever was pairing.
            // `persist` writes the same 32 raw bytes `identity/user.key`
            // holds, mode 0600, so the receiving system needs no decoding
            // step and no instructions beyond the path.
            let path = std::path::PathBuf::from(out.unwrap_or_else(|| format!("{name}.key")));
            if path.exists() {
                bail!(
                    "{} already exists — refusing to overwrite a key file.\n\n\
                     The account has been created; re-run with --out <path> pointing \
                     somewhere else to write its credential, or delete that file if you \
                     are sure it is not in use.",
                    path.display()
                );
            }
            kp.persist(&path)
                .with_context(|| format!("could not write {}", path.display()))?;

            println!("service account '{name}' created");
            println!("  identity   {}", user_id.to_display_string());
            println!("  role       {} on {nodes}", role.as_str());
            println!(
                "  expires    {}",
                match expires_at_micros {
                    Some(e) => mesh_core::localtime::describe_instant(e),
                    None => "never — this is a long-lived credential".into(),
                }
            );
            println!("  key        {}  (mode 0600)", path.display());
            println!();
            println!("meshware does not store that key and cannot show it to you again.");
            println!("Put it where the system that needs it will read it, then delete your copy:");
            println!();
            println!(
                "  MESHWARE_USER_IDENTITY=/path/to/{} meshnode run …",
                path.file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("service.key")
            );
            println!();
            println!("Revoke it like any other principal: meshnode users revoke {name} --yes");
            Ok(0)
        }
        ServiceAccountsCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::ServiceAccounts).await?;
            let IpcResponse::ServiceAccounts(rows) = resp else {
                return unexpected(resp);
            };
            format::print_service_accounts(&rows, output == Output::Json)?;
            Ok(0)
        }
    }
}

async fn run_users_cmd(ctx: &Ctx, cmd: UsersCmd) -> Result<i32> {
    match cmd {
        UsersCmd::Add {
            username,
            key,
            role,
            nodes,
            services: services_glob,
            expires,
        } => {
            let user_id = UserId::parse(&key).context("invalid --key (expected mwu1…)")?;
            let role = parse_role(&role)?;
            let expires_at_micros = parse_expiry(expires)?;
            let mut record = UserRecord {
                user_id,
                username: username.clone(),
                created_at_micros: mesh_proto::now_micros(),
                added_by: ctx.user_id(),
                signature: Vec::new(),
            };
            record.signature = ctx.sign(&record.signing_bytes())?;
            ctx.ipc(IpcRequest::UserAdd { record }).await?;
            let grant = build_grant(ctx, user_id, role, nodes, services_glob, expires_at_micros)?;
            ctx.ipc(IpcRequest::GrantAdd { grant }).await?;
            println!("user '{username}' added with role {}", role.as_str());
            Ok(0)
        }
        UsersCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Users).await?;
            let IpcResponse::Users(rows) = resp else {
                return unexpected(resp);
            };
            format::print_users(&rows, output == Output::Json)?;
            Ok(0)
        }
        UsersCmd::Pending { output } => {
            let IpcResponse::Enrolments(rows) = ctx.ipc(IpcRequest::Enrolments).await? else {
                bail!("unexpected response to users pending");
            };
            format::print_enrolments(&rows, output == Output::Json)?;
            Ok(0)
        }
        UsersCmd::Approve {
            request,
            username,
            role,
            nodes,
            services: services_glob,
            expires,
        } => {
            let row = resolve_enrolment(ctx, &request).await?;
            let username = username.unwrap_or_else(|| row.suggested_username.clone());
            if username.is_empty() {
                bail!("this request suggested no username — pass --username");
            }
            let role = parse_role(&role)?;
            let expires_at_micros = parse_expiry(expires)?;
            // Approving *is* `UserAdd` + `GrantAdd`, unchanged and still
            // admin-signed. The pending row is context for the human, not
            // a new authorization path — which is what makes the queue
            // safe to replicate in the first place (M23 §3.2).
            let mut record = UserRecord {
                user_id: row.user_id,
                username: username.clone(),
                created_at_micros: mesh_proto::now_micros(),
                added_by: ctx.user_id(),
                signature: Vec::new(),
            };
            record.signature = ctx.sign(&record.signing_bytes())?;
            ctx.ipc(IpcRequest::UserAdd { record }).await?;
            let grant = build_grant(
                ctx,
                row.user_id,
                role,
                nodes,
                services_glob,
                expires_at_micros,
            )?;
            ctx.ipc(IpcRequest::GrantAdd { grant }).await?;
            println!(
                "approved {} ({}) as '{username}' with role {}",
                row.code,
                row.user_id.short(),
                role.as_str()
            );
            Ok(0)
        }
        UsersCmd::AttestRoot { cmd } => match cmd {
            AttestRootCmd::Ls { output } => {
                let IpcResponse::AttestRoots(rows) = ctx.ipc(IpcRequest::AttestRoots).await? else {
                    bail!("could not read the trusted attestation authorities");
                };
                format::print_attest_roots(&rows, output == Output::Json)?;
                Ok(0)
            }
            AttestRootCmd::Add { cert, label } => {
                let certificate =
                    std::fs::read(&cert).with_context(|| format!("reading {}", cert.display()))?;
                if certificate.starts_with(b"-----BEGIN") {
                    bail!(
                        "{} is PEM; this wants DER.\n  openssl x509 -in {} -outform der -out \
                         ca.der",
                        cert.display(),
                        cert.display()
                    );
                }
                // A label is what everybody will read this by, so an empty
                // one falls back to the filename rather than to a blank
                // cell — and never to the id, which is a digest.
                let label = if label.trim().is_empty() {
                    cert.file_stem()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attestation authority".into())
                } else {
                    label
                };
                let mut root = mesh_proto::AttestRoot {
                    label: label.clone(),
                    certificate,
                    added_by: ctx.user_id(),
                    added_at_micros: mesh_proto::now_micros(),
                    signature: Vec::new(),
                };
                root.signature = ctx.sign(&root.signing_bytes())?;
                let id = root.id();
                let resp = ctx
                    .ipc(IpcRequest::AttestRootAdd {
                        root: Box::new(root),
                    })
                    .await?;
                match resp {
                    IpcResponse::Ok => {
                        println!("Trusting {label:?} ({}).", &id[..12.min(id.len())]);
                        println!(
                            "Anyone whose key it vouches for can now publish a chain:  \
                             meshnode identity hardware attest <cert>…"
                        );
                        Ok(0)
                    }
                    IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                        bail!(message)
                    }
                    other => unexpected(other),
                }
            }
            AttestRootCmd::Rm { id } => {
                let at_micros = mesh_proto::now_micros();
                let by = ctx.user_id();
                let signature = ctx.sign(&mesh_proto::attest_root_remove_signing_bytes(
                    &id, &by, at_micros,
                ))?;
                let resp = ctx
                    .ipc(IpcRequest::AttestRootRemove {
                        id: id.clone(),
                        by,
                        at_micros,
                        signature,
                    })
                    .await?;
                match resp {
                    IpcResponse::Ok => {
                        println!("No longer trusted. Attestations it vouched for stop verifying.");
                        Ok(0)
                    }
                    IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                        bail!(message)
                    }
                    other => unexpected(other),
                }
            }
        },
        UsersCmd::Decline { request, reason } => {
            // Refused here rather than truncated, and dropped at merge if a
            // client skips this: the reason is what the next admin reads,
            // and half a sentence is worse than being asked to shorten it.
            if reason.len() > mesh_proto::enrol::MAX_DECLINE_REASON {
                bail!(
                    "that reason is {} characters; the limit is {}",
                    reason.len(),
                    mesh_proto::enrol::MAX_DECLINE_REASON
                );
            }
            let row = resolve_enrolment(ctx, &request).await?;
            let mut decline = mesh_proto::EnrolmentDecline {
                user_id: row.user_id,
                reason,
                declined_by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            decline.signature = ctx.sign(&decline.signing_bytes())?;
            ctx.ipc(IpcRequest::EnrolDecline {
                decline: Box::new(decline),
            })
            .await?;
            println!("declined {} ({})", row.code, row.user_id.short());
            Ok(0)
        }
        UsersCmd::Rotate {
            username,
            new_key,
            reason,
        } => {
            let new_id = UserId::parse(&new_key).context("invalid --new-key (expected mwu1…)")?;
            // Find the key being replaced, and the grants riding on it.
            let IpcResponse::Users(users) = ctx.ipc(IpcRequest::Users).await? else {
                bail!("could not read the user list");
            };
            let old = users
                .iter()
                .find(|u| u.username == username && !u.revoked)
                .with_context(|| format!("no active user called {username:?}"))?;
            if old.user_id == new_id {
                bail!("that is the key {username} already holds — nothing to rotate");
            }
            let IpcResponse::Grants(grants) = ctx.ipc(IpcRequest::Grants).await? else {
                bail!("could not read grants");
            };
            let carried: Vec<mesh_proto::Grant> = grants
                .iter()
                .filter(|g| g.user_id == old.user_id && !g.revoked && !g.expired)
                .map(|g| -> Result<mesh_proto::Grant> {
                    let mut grant = mesh_proto::Grant {
                        grant_id: mesh_proto::GrantId::generate(),
                        user_id: new_id,
                        role: parse_role(&g.role).unwrap_or(mesh_proto::Role::Viewer),
                        nodes: g.nodes.clone(),
                        services: g.services.clone(),
                        expires_at_micros: g.expires_at_micros,
                        created_at_micros: mesh_proto::now_micros(),
                        granted_by: ctx.user_id(),
                        signature: Vec::new(),
                    };
                    grant.signature = ctx.sign(&grant.signing_bytes())?;
                    Ok(grant)
                })
                .collect::<Result<_>>()?;

            let mut record = UserRecord {
                user_id: new_id,
                username: username.clone(),
                created_at_micros: mesh_proto::now_micros(),
                added_by: ctx.user_id(),
                signature: Vec::new(),
            };
            record.signature = ctx.sign(&record.signing_bytes())?;

            let mut revocation = mesh_proto::UserRevocation {
                user_id: old.user_id,
                reason: reason.clone(),
                revoked_by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            revocation.signature = ctx.sign(&revocation.signing_bytes())?;

            let n = carried.len();
            ctx.ipc(IpcRequest::UserRotateKey {
                username: username.clone(),
                record: Box::new(record),
                grants: carried,
                revocation: Box::new(revocation),
            })
            .await?;
            println!("{username}: key rotated");
            println!("  was      {}", old.user_id.short());
            println!("  now      {}", new_id.short());
            println!("  carried  {n} grant(s)");
            println!();
            println!("The old key is revoked and can never be re-enrolled.");
            println!("Their audit history stays under it, and stays readable.");
            Ok(0)
        }
        UsersCmd::Revoke { user, reason, yes } => {
            if !yes {
                bail!("user revocation is permanent — re-run with --yes");
            }
            let user_id = resolve_user(ctx, &user).await?;
            let mut revocation = UserRevocation {
                user_id,
                reason,
                revoked_by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            revocation.signature = ctx.sign(&revocation.signing_bytes())?;
            ctx.ipc(IpcRequest::UserRevoke { revocation }).await?;
            println!("user {} revoked", user_id.short());
            Ok(0)
        }
    }
}

async fn run_grants_cmd(ctx: &Ctx, cmd: GrantsCmd) -> Result<i32> {
    match cmd {
        GrantsCmd::Add {
            user,
            role,
            nodes,
            services: services_glob,
            expires,
        } => {
            let user_id = resolve_user(ctx, &user).await?;
            let role = parse_role(&role)?;
            let expires_at_micros = parse_expiry(expires)?;
            let grant = build_grant(ctx, user_id, role, nodes, services_glob, expires_at_micros)?;
            let grant_id = grant.grant_id;
            ctx.ipc(IpcRequest::GrantAdd { grant }).await?;
            println!("grant {} added", grant_id);
            Ok(0)
        }
        GrantsCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Grants).await?;
            let IpcResponse::Grants(rows) = resp else {
                return unexpected(resp);
            };
            format::print_grants(&rows, output == Output::Json)?;
            Ok(0)
        }
        GrantsCmd::Revoke { grant, yes } => {
            if !yes {
                bail!("re-run with --yes to revoke this grant");
            }
            let grant_id = GrantId::parse(&grant).context("invalid grant id (grant-…)")?;
            let mut revocation = GrantRevocation {
                grant_id,
                revoked_by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            revocation.signature = ctx.sign(&revocation.signing_bytes())?;
            ctx.ipc(IpcRequest::GrantRevoke { revocation }).await?;
            println!("grant {grant_id} revoked");
            Ok(0)
        }
    }
}

async fn run_secrets_cmd(ctx: &Ctx, cmd: SecretsCmd) -> Result<i32> {
    match cmd {
        SecretsCmd::Scope { name, scope, clear } => {
            if clear == scope.is_set() {
                bail!("say what the scope is (--on, --user, --role), or --clear it");
            }
            publish_scope(ctx, &name, &scope).await?;
            if clear {
                println!("secret '{name}' scope cleared — the default nodes, and any user");
            } else {
                println!(
                    "secret '{name}' scoped. Nodes it no longer names keep this value until it \
                     is set again."
                );
            }
            Ok(0)
        }
        SecretsCmd::Set {
            name,
            value,
            from_file,
            scope,
        } => {
            let value: Vec<u8> = match (value, from_file) {
                (Some(v), None) => v.into_bytes(),
                (None, Some(path)) => {
                    std::fs::read(&path).with_context(|| format!("reading {path}"))?
                }
                (None, None) => {
                    eprintln!("reading secret value from stdin (end with EOF)…");
                    let mut buf = Vec::new();
                    std::io::stdin()
                        .read_to_end(&mut buf)
                        .context("reading stdin")?;
                    // Trim one trailing newline (almost always an artifact).
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                    }
                    buf
                }
                (Some(_), Some(_)) => bail!("--value and --from-file are mutually exclusive"),
            };
            if value.is_empty() {
                bail!("refusing to store an empty secret");
            }

            // Recipients: the nodes this secret is for (SEC-7) — by the
            // scope about to be set, when one is, else by the one it has —
            // so whichever of them runs a matching job can decrypt. Nodes
            // that join later are healed by rewrap; a quarantined one is not
            // (SEC-16). Asked before anything is published, so a scope that
            // names no node leaves nothing behind.
            let proposed = scope.is_set().then(|| scope.on.clone().unwrap_or_default());
            let resp = ctx
                .ipc(IpcRequest::SecretRecipients {
                    name: name.clone(),
                    nodes: proposed,
                })
                .await?;
            let IpcResponse::SecretRecipients(recipients) = resp else {
                return unexpected(resp);
            };
            if recipients.is_empty() {
                bail!("no node is in this secret's scope — nothing could ever decrypt it");
            }
            if scope.is_set() {
                publish_scope(ctx, &name, &scope).await?;
            }

            // Next version, counting tombstones (LWW is by version, hlc).
            let resp = ctx.ipc(IpcRequest::Secrets).await?;
            let IpcResponse::Secrets(rows) = resp else {
                return unexpected(resp);
            };
            let version = rows
                .iter()
                .filter(|r| r.name == name)
                .map(|r| r.version)
                .max()
                .unwrap_or(0)
                + 1;

            // Seal locally: the plaintext never leaves this process.
            let mut record =
                secrets::seal_secret(&name, version, &value, &recipients, ctx.user_id())?;
            record.signature = ctx.sign(&record.signing_bytes())?;
            ctx.ipc(IpcRequest::SecretPublish { record }).await?;
            println!(
                "secret '{name}' v{version} sealed for {} node(s) — reference it as {{{{secret:{name}}}}}",
                recipients.len()
            );
            Ok(0)
        }
        SecretsCmd::Ls { output } => {
            let resp = ctx.ipc(IpcRequest::Secrets).await?;
            let IpcResponse::Secrets(rows) = resp else {
                return unexpected(resp);
            };
            let scopes = match ctx.ipc(IpcRequest::SecretScopes).await? {
                IpcResponse::SecretScopes(s) => s,
                _ => Vec::new(),
            };
            format::print_secrets(&rows, &scopes, output == Output::Json)?;
            Ok(0)
        }
        SecretsCmd::Rm { name, yes } => {
            if !yes {
                bail!("re-run with --yes to remove this secret");
            }
            let mut removal = SecretRemoval {
                name: name.clone(),
                by: ctx.user_id(),
                at_micros: mesh_proto::now_micros(),
                signature: Vec::new(),
            };
            removal.signature = ctx.sign(&removal.signing_bytes())?;
            ctx.ipc(IpcRequest::SecretRemove { removal }).await?;
            println!("secret '{name}' removed");
            Ok(0)
        }
        SecretsCmd::Providers { output } => {
            let resp = ctx.ipc(IpcRequest::SecretProviders).await?;
            let IpcResponse::Providers(rows) = resp else {
                return unexpected(resp);
            };
            format::print_providers(&rows, output == Output::Json)?;
            Ok(0)
        }
    }
}

/// Resolve a bundle argument (full id or unique prefix) against the mesh.
/// A bundle by id, by unique prefix, or by the name of a service that
/// binds one (M24 §3.2).
///
/// The service name is the ref people actually have: nobody remembers
/// `bndl-4f2a…`, and "push `deploy-api` to the edge" is the sentence being
/// typed.
async fn resolve_bundle_ref(ctx: &Ctx, arg: &str) -> Result<mesh_proto::BundleId> {
    if let Ok(id) = mesh_proto::BundleId::parse(arg) {
        return Ok(id);
    }
    if let IpcResponse::Services(rows) = ctx.ipc(IpcRequest::Services).await?
        && let Some(row) = rows.iter().find(|r| r.name == arg)
    {
        return Ok(row.bundle_id);
    }
    resolve_bundle(ctx, arg).await
}

/// `meshnode upgrade --publish` (M24 §9): put a signed binary into the
/// mesh so nodes fetch it from each other.
///
/// This is the answer to "why does a mesh with no central server need a
/// central server to upgrade itself". The binary becomes an ordinary
/// content-addressed bundle — replicated lazily, pulled by digest,
/// chunk-verified — and the record naming it carries the release-key
/// signature every node checks before installing anything.
/// Trust a release key mesh-wide: accepts a `.pub` file or the bare key
/// line, because an operator has one or the other in front of them and
/// should not have to know which one this wants.
async fn trust_release_key(ctx: &Ctx, source: &str, label: Option<&str>) -> Result<i32> {
    let raw = match std::fs::read_to_string(source) {
        Ok(text) => text,
        // Not a path: take it as the key itself.
        Err(_) => source.to_string(),
    };
    // A .pub is two lines, the first an untrusted comment. Taking the last
    // key-shaped line means both a file and a paste land in the same place.
    let pubkey = raw
        .lines()
        .map(str::trim)
        .rfind(|l: &&str| !l.is_empty() && !l.starts_with("untrusted comment"))
        .unwrap_or("")
        .to_string();
    // Y6. One trusted list, two kinds of key: a minisign line starts `RW`,
    // an SSH one names its algorithm first, and nothing can confuse them.
    // Adding the first SSH key is what turns the co-signature requirement
    // on for the whole fleet, so say so rather than leaving it to be
    // discovered at the next upgrade.
    let is_ssh = mesh_core::sshsig::is_ssh_key_line(&pubkey);
    if !is_ssh && !mesh_proto::ReleaseKey::looks_like_a_key(&pubkey) {
        bail!(
            "no release key in {source:?} — expected the `RW…` line of a minisign .pub \
             (generate one with `minisign -G`), or an `ssh-ed25519`/`sk-ssh-ed25519` public \
             key line for a hardware co-signature"
        );
    }
    let label = label
        .map(str::to_string)
        .unwrap_or_else(|| match std::fs::canonicalize(source) {
            Ok(p) => p.display().to_string(),
            Err(_) => "release key".to_string(),
        });
    let mut key = mesh_proto::ReleaseKey {
        pubkey: pubkey.clone(),
        label,
        added_by: ctx.user_id(),
        added_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    key.signature = ctx.sign(&key.signing_bytes())?;
    match ctx
        .ipc(IpcRequest::ReleaseKeyAdd { key: Box::new(key) })
        .await?
    {
        IpcResponse::Ok => {
            println!("this mesh now trusts {pubkey}");
            if is_ssh {
                println!();
                println!(
                    "This is a co-signing key, so **every release now needs a second \
                     signature** made"
                );
                println!(
                    "with it — a minisign signature alone will no longer install anywhere \
                     in this mesh."
                );
                println!(
                    "  Sign one:     ssh-keygen -Y sign -f <key> -n {} meshnode",
                    mesh_core::sshsig::RELEASE_NAMESPACE
                );
                println!("  Publish it:   meshnode upgrade --cosign meshnode.sig --version <v>");
            }
            println!();
            println!(
                "Every node verifies releases against it — including nodes that join later. \
                 No per-machine configuration."
            );
            Ok(0)
        }
        other => bail!("{other:?}"),
    }
}

/// `meshnode upgrade --cosign <file> --version <v>` (Y6).
///
/// Publishes the second signature — the one a stolen CI secret cannot
/// produce, because it was made by a key on a device somebody touched.
///
/// The release must already be published: a co-signature is *about* a
/// binary, and the digest it names is read from the release record rather
/// than recomputed here, so a co-signature can never end up attached to a
/// binary nobody has.
async fn publish_cosignature(ctx: &Ctx, sig_file: &str, version: &str) -> Result<i32> {
    let armored =
        std::fs::read_to_string(sig_file).with_context(|| format!("reading {sig_file}"))?;
    // Parsed here so a wrong file is a sentence rather than an op every
    // node silently declines to record.
    let parsed = mesh_core::sshsig::parse(&armored)?;
    if parsed.namespace != mesh_core::sshsig::RELEASE_NAMESPACE {
        bail!(
            "that signature was made for {:?}. Releases are signed under {:?}:\n  ssh-keygen \
             -Y sign -f <key> -n {} meshnode",
            parsed.namespace,
            mesh_core::sshsig::RELEASE_NAMESPACE,
            mesh_core::sshsig::RELEASE_NAMESPACE,
        );
    }

    let bare = version.trim().trim_start_matches(['v', 'V']).to_string();
    let IpcResponse::Releases(rows) = ctx.ipc(IpcRequest::Releases).await? else {
        bail!("unexpected response to releases");
    };
    let matching: Vec<&mesh_proto::ReleaseRow> =
        rows.iter().filter(|r| r.version == bare).collect();
    let digest_hex = match matching.as_slice() {
        [one] => one.digest.clone(),
        [] => bail!(
            "this mesh holds no release {bare} — publish the binary first:\n  meshnode upgrade \
             --publish meshnode --version {bare}"
        ),
        // One version, several architectures: each is a different binary
        // and therefore a different signature. Naming the ambiguity beats
        // co-signing whichever happened to sort first.
        many => bail!(
            "release {bare} has {} architectures, and each binary needs its own co-signature. \
             Publish one per artifact: {}",
            many.len(),
            many.iter()
                .map(|r| r.artifact.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let digest = mesh_proto::parse_hex32(&digest_hex)
        .ok_or_else(|| anyhow!("the daemon returned an unreadable release digest"))?;

    let mut cosign = mesh_proto::ReleaseCosignature {
        digest,
        armored,
        published_by: ctx.user_id(),
        published_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    cosign.signature = ctx.sign(&cosign.signing_bytes())?;
    match ctx
        .ipc(IpcRequest::ReleaseCosign {
            cosign: Box::new(cosign),
        })
        .await?
    {
        IpcResponse::Ok => {
            println!("Co-signature published for {bare}.");
            if parsed.is_token_backed() {
                println!(
                    "  Made by a FIDO2 key{}.",
                    if parsed.user_present() {
                        ", with a touch recorded"
                    } else {
                        ""
                    }
                );
            }
            Ok(0)
        }
        IpcResponse::Denied { message } | IpcResponse::Error { message } => bail!(message),
        other => unexpected(other),
    }
}

async fn publish_release(
    ctx: &Ctx,
    binary: &str,
    sig_path: Option<&str>,
    version: &str,
) -> Result<i32> {
    let binary = std::path::PathBuf::from(binary);
    let bytes = std::fs::read(&binary).with_context(|| format!("reading {}", binary.display()))?;
    let bare = version.trim().trim_start_matches(['v', 'V']).to_string();

    // Refuse early rather than on every node at once: the upgrader runs
    // `--version` on what it installs and rejects a mismatch, so a wrong
    // binary here is a fleet-wide no-op discovered one node at a time.
    let printed = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", binary.display()))?;
    let printed = String::from_utf8_lossy(&printed.stdout).trim().to_string();
    if !printed.contains(&bare) {
        bail!(
            "{} reports {printed:?}, which does not contain {bare} — every node would fetch it, \
             refuse it, and leave itself untouched",
            binary.display()
        );
    }

    let sig_path = sig_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(format!("{}.minisig", binary.display())));
    let minisig = std::fs::read_to_string(&sig_path).with_context(|| {
        format!(
            "reading {} — a release must carry its signature, or no node will install it",
            sig_path.display()
        )
    })?;

    // Verified here too, and against the keys the *fleet* trusts rather
    // than whatever this one machine has in a file: publishing a release
    // no node will install is the failure this check exists to prevent,
    // and it is the fleet's answer that decides that, not the publisher's.
    let trusted = match ctx.ipc(IpcRequest::ReleaseKeys).await {
        Ok(IpcResponse::ReleaseKeys(view)) => view
            .keys
            .into_iter()
            .filter(|k| !k.revoked)
            .map(|k| k.pubkey)
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let candidates = match &ctx.config.release.pubkey {
        Some(local) => vec![local.clone()],
        None => trusted,
    };
    let pubkeys = meshware_node::upgrade::release_pubkeys(&candidates)?;
    meshware_node::upgrade::verify_minisign_any(&bytes, &minisig, &pubkeys).context(
        "this release is signed by a key the mesh does not trust — add it with \
         `meshnode upgrade --trust-key <file.pub>`",
    )?;

    // The bundle is one file: the binary under its release artifact name.
    // Staged beside the source, because the *daemon* ingests it and has to
    // be able to read the path this names.
    // Named for the binary's architecture, read off its file name, not for
    // this machine's: publishing an aarch64 build from an x86_64 laptop is
    // the normal way a mixed fleet gets its second artifact.
    let arch = binary
        .file_name()
        .and_then(|n| mesh_core::deploy::arch_of_artifact(&n.to_string_lossy()).map(str::to_string))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let artifact = mesh_core::deploy::artifact_name(&bare, &arch);
    let staging = binary
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(format!(".meshnode-release-{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("staging in {}", staging.display()))?;
    let staged = || {
        let _ = std::fs::remove_dir_all(&staging);
    };
    if let Err(e) = std::fs::copy(&binary, staging.join(&artifact)) {
        staged();
        return Err(anyhow::Error::from(e).context("staging the release binary"));
    }
    let abs = match std::fs::canonicalize(&staging) {
        Ok(p) => p,
        Err(e) => {
            staged();
            return Err(anyhow::Error::from(e).context("resolving the staging directory"));
        }
    };
    let pushed = ctx
        .ipc(IpcRequest::Push {
            path: abs.to_string_lossy().into_owned(),
        })
        .await;
    staged();
    let IpcResponse::Pushed {
        bundle_id,
        total_size,
        ..
    } = pushed?
    else {
        bail!("unexpected response to push");
    };

    let mut release = mesh_proto::ReleaseRecord {
        version: bare.clone(),
        bundle_id,
        artifact: artifact.clone(),
        minisig,
        digest: *blake3::hash(&bytes).as_bytes(),
        published_by: ctx.user_id(),
        published_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    release.signature = ctx.sign(&release.signing_bytes())?;
    ctx.ipc(IpcRequest::ReleasePublish {
        release: Box::new(release),
    })
    .await?;

    println!(
        "published meshnode {bare} into the mesh ({}, {})",
        bundle_id.short(),
        format::bytes(total_size)
    );
    println!();
    println!("Nodes fetch it from each other — no server, nothing to be down. Warm the fleet");
    println!("first if you want the rollout to be instant:");
    println!(
        "  meshnode bundles prefetch {} --on tag=<ring>",
        bundle_id.short()
    );
    println!("  meshnode upgrade --on tag=<ring> --to {bare} --batch 8");
    Ok(0)
}

/// `meshnode upgrade --on <selector> --to <version>` (M24 §4).
///
/// The rollout is driven by the local daemon, which is what lets it health
/// gate on the op-log reading it already has. This process only prints —
/// and, deliberately, prints every step, because a rollout you cannot watch
/// is a rollout you cannot stop.
async fn run_fleet_upgrade(
    ctx: &Ctx,
    selector: String,
    version: String,
    batch: u32,
    move_pending: bool,
    force: bool,
    yes: bool,
) -> Result<i32> {
    // Every spelling of it, not just the bare word: `any|name=anvil` is
    // the whole mesh with a decoration on it (M24 §10).
    if mesh_core::membership::Selector::parse(&selector).is_unbounded() {
        bail!(
            "refusing --on any: name a ring (--on tag=canary), watch it, then widen. \
             Upgrading a whole mesh in one command is the thing this is built not to do."
        );
    }
    if !yes {
        eprintln!(
            "About to roll meshnode {version} across {selector}, {}, waiting for each to come \
             back alive and caught up before starting the next.",
            if batch <= 1 {
                "one node at a time".to_string()
            } else {
                format!("{batch} at a time")
            }
        );
        if force {
            eprintln!(
                "--force: nodes already reporting {version} will reinstall it rather than be \
                 skipped. The release signature is still verified on every one of them."
            );
        }
        eprintln!(
            "The rollout stops at the first failure and leaves the rest alone — a mesh \
             mid-upgrade keeps working."
        );
        if !confirm_tty("continue?") {
            bail!("aborted");
        }
    }
    // Client-built and client-signed, like every other governance body: the
    // daemon does not hold the user's key, and this signature is what every
    // node in the ring re-verifies against its own state.
    let now = mesh_proto::now_micros();
    let mut plan = mesh_proto::UpgradePlan {
        plan_id: mesh_proto::UpgradeId::generate(),
        selector: selector.clone(),
        version: version.trim().trim_start_matches(['v', 'V']).to_string(),
        move_pending,
        batch,
        created_by: ctx.user_id(),
        created_at_micros: now,
        expires_at_micros: now + mesh_proto::DEFAULT_PLAN_TTL_SECS * 1_000_000,
        signature: Vec::new(),
    };
    // Signed over the forced domain when it is a forced plan: forcing is
    // not a field of the record — it is which op body carries it — so
    // without this an ordinary plan could be republished as a forced one
    // by anyone who saw it gossip past.
    plan.signature = ctx.sign(&plan.signing_bytes_forced(force))?;
    let plan_id = plan.plan_id;
    let mut stream = ctx
        .stream(IpcRequest::UpgradeFleet {
            plan: Box::new(plan),
            force,
        })
        .await?;
    // The plan id is printed once the rollout is actually under way — a
    // refusal should not leave somebody holding an id for a plan that
    // never authorized anything.
    let mut announced = false;
    let mut announce = || {
        if !announced {
            announced = true;
            println!(
                "{}",
                term::out().prose(&format!(
                    "plan {plan_id} — withdraw it with `meshnode upgrade --cancel {plan_id}`"
                ))
            );
        }
    };
    loop {
        let resp: IpcResponse = match read_frame(&mut stream).await {
            Ok(r) => r,
            // The driver upgrades itself last and then goes down; a closed
            // stream after that frame is the expected ending, not an error.
            Err(mesh_proto::ProtoError::Io(_)) => break,
            Err(e) => return Err(e.into()),
        };
        match resp {
            IpcResponse::UpgradeProgress {
                node,
                state,
                detail,
            } => {
                announce();
                println!("{node:<20} {state:<10} {detail}");
            }
            IpcResponse::UpgradeDone {
                upgraded,
                already,
                driver_restarting,
                remaining,
                stopped_at,
            } => {
                announce();
                println!();
                if driver_restarting {
                    // Said before the counts, because it is the part that
                    // is not finished yet.
                    println!(
                        "This node is draining and restarting into {}; it is not counted below \
                         because it cannot verify itself.",
                        version.trim().trim_start_matches(['v', 'V'])
                    );
                }
                match stopped_at {
                    Some(node) => {
                        println!(
                            "stopped at {node}: {upgraded} upgraded, {already} already current, \
                             {remaining} not started."
                        );
                        println!(
                            "A partial upgrade is a supported state — the op-log is \
                             forward-compatible, so the mesh keeps working. Fix {node}, then \
                             run this again."
                        );
                        return Ok(1);
                    }
                    None => {
                        println!(
                            "{upgraded} upgraded and verified back, {already} already current."
                        );
                        if driver_restarting {
                            println!(
                                "{}",
                                term::out()
                                    .prose("Confirm this node with `meshnode upgrade --status`.")
                            );
                        }
                        return Ok(0);
                    }
                }
            }
            IpcResponse::Error { message } => bail!("{message}"),
            IpcResponse::Denied { message } => bail!("permission denied: {message}"),
            other => return unexpected(other),
        }
    }
    Ok(0)
}

fn confirm_tty(question: &str) -> bool {
    use std::io::Write as _;
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// `meshnode bundles prefetch` — one line per node, rewritten in place as
/// each reports (M24 §3.2).
async fn run_prefetch(
    ctx: &Ctx,
    bundle_id: mesh_proto::BundleId,
    selector: String,
    json: bool,
) -> Result<i32> {
    let mut stream = ctx
        .stream(IpcRequest::BundlePrefetch {
            bundle_id,
            selector: selector.clone(),
        })
        .await?;
    if !json {
        eprintln!(
            "pushing {} to {selector} — nodes that miss it still fetch it when they need it…",
            bundle_id.short()
        );
    }
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut exit = 0;
    loop {
        let resp: IpcResponse = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(mesh_proto::ProtoError::Io(_)) => break,
            Err(e) => return Err(e.into()),
        };
        match resp {
            IpcResponse::PrefetchProgress {
                node,
                progress,
                unreachable,
            } => {
                let line = match &unreachable {
                    Some(why) => format!("not pushed: {why}"),
                    None if progress.complete => {
                        format!("has it ({})", format::bytes(progress.total_bytes))
                    }
                    None if progress.total_chunks == 0 => "starting…".to_string(),
                    None => format!(
                        "{}/{} chunks ({})",
                        progress.have_chunks,
                        progress.total_chunks,
                        format::bytes(progress.have_bytes)
                    ),
                };
                match rows.iter_mut().find(|(n, _)| *n == node) {
                    Some(row) => row.1 = line,
                    None => rows.push((node, line)),
                }
            }
            IpcResponse::PrefetchDone {
                complete,
                fetching,
                skipped,
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "bundle": bundle_id.to_display_string(),
                            "selector": selector,
                            "complete": complete,
                            "fetching": fetching,
                            "skipped": skipped,
                            "nodes": rows.iter().map(|(n, s)| serde_json::json!({"node": n, "state": s})).collect::<Vec<_>>(),
                        })
                    );
                } else {
                    for (node, line) in &rows {
                        println!("{node:<20} {line}");
                    }
                    println!();
                    println!("{complete} have it, {fetching} still fetching, {skipped} not pushed");
                }
                // Still-fetching is not a failure — the fetch outlives this
                // command. Not-pushed is not one either: those nodes fetch
                // lazily, exactly as they always did.
                exit = 0;
                break;
            }
            IpcResponse::Error { message } => bail!("{message}"),
            IpcResponse::Denied { message } => bail!("permission denied: {message}"),
            other => return unexpected(other),
        }
    }
    Ok(exit)
}

async fn resolve_bundle(ctx: &Ctx, arg: &str) -> Result<mesh_proto::BundleId> {
    if let Ok(id) = mesh_proto::BundleId::parse(arg) {
        return Ok(id);
    }
    let IpcResponse::Bundles(rows) = ctx.ipc(IpcRequest::Bundles).await? else {
        bail!("could not list bundles to resolve {arg:?}");
    };
    let matches: Vec<mesh_proto::BundleId> = rows
        .iter()
        .filter(|r| r.bundle_id.to_display_string().starts_with(arg))
        .map(|r| r.bundle_id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!("no bundle matches {arg:?}"),
        many => bail!("{arg:?} is ambiguous ({} bundles match)", many.len()),
    }
}

/// Resolve a fan-out argument (full id or unique prefix) against this
/// node's own result sets (M19 §5.3).
///
/// A prefix, because the short form is what `run --fanout` printed and
/// therefore what somebody has in front of them. Resolved against the
/// *local* index, which is the honest scope: rows live on the node that
/// owns the group, so a prefix that matches nothing here is either a group
/// this node never owned or one whose rows were never captured — and the
/// full-id path below still reaches the refusal that says which.
async fn resolve_fanout(ctx: &Ctx, arg: &str) -> Result<mesh_proto::FanId> {
    if let Ok(id) = mesh_proto::FanId::parse(arg) {
        return Ok(id);
    }
    let IpcResponse::ResultSets(sets) = ctx
        .ipc(IpcRequest::ResultsExport {
            since_micros: None,
            limit: 500,
        })
        .await?
    else {
        bail!("{arg:?} is not a fan-out id, and this node has no result sets to match it against");
    };
    let matches: Vec<mesh_proto::FanId> = sets
        .iter()
        .filter(|s| s.group.to_display_string().starts_with(arg))
        .map(|s| s.group)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!(
            "no fan-out here matches {arg:?}. Rows live on the node that ran the \
             fan-out, and only for a run launched with --json, --outputs or --expect"
        ),
        many => bail!("{arg:?} is ambiguous ({} fan-outs match)", many.len()),
    }
}

/// Resolve a node argument (full id or unique prefix) against the mesh.
/// The member this argument names, if it names one. Unlike
/// [`resolve_node`] a miss is not an error: the caller has another reading
/// of the argument to fall back to (a hostname, a raw address).
async fn node_target(ctx: &Ctx, arg: &str) -> Option<NodeId> {
    if arg.contains(':') {
        return None; // host:port — an address, never a node name
    }
    resolve_node(ctx, arg).await.ok()
}

async fn resolve_node(ctx: &Ctx, arg: &str) -> Result<NodeId> {
    if let Ok(id) = NodeId::parse(arg) {
        return Ok(id);
    }
    let IpcResponse::Nodes(rows) = ctx.ipc(IpcRequest::Nodes).await? else {
        bail!("could not list nodes to resolve {arg:?}");
    };
    let matches: Vec<NodeId> = rows
        .iter()
        .filter(|r| r.node_id.to_display_string().starts_with(arg) || r.name == arg)
        .map(|r| r.node_id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!("no node matches {arg:?}"),
        many => bail!("{arg:?} is ambiguous ({} nodes match)", many.len()),
    }
}

/// Resolve a user argument (username, full id, or id prefix).
/// Resolve a pending request from what an admin actually has in front of
/// them: the code somebody read out over the phone, the key, or the name
/// they asked to be called.
async fn resolve_enrolment(ctx: &Ctx, arg: &str) -> Result<mesh_proto::EnrolmentRow> {
    let IpcResponse::Enrolments(rows) = ctx.ipc(IpcRequest::Enrolments).await? else {
        bail!("could not list pending requests to resolve {arg:?}");
    };
    let needle = arg.trim().to_ascii_uppercase();
    let matches: Vec<mesh_proto::EnrolmentRow> = rows
        .into_iter()
        .filter(|r| {
            r.code == needle
                || r.user_id.to_display_string().starts_with(arg)
                || r.suggested_username == arg
        })
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one")),
        0 => bail!("no pending request matches {arg:?} (see `meshnode users pending`)"),
        n => bail!("{arg:?} is ambiguous ({n} pending requests match) — use the code"),
    }
}

/// `meshnode enrol` (M23 §3.1): ask, show the code, and then *say what
/// happened*. The silence after step 5 is the bug this command exists to
/// close, so the waiting half is not optional decoration.
async fn run_enrol(
    global: &GlobalArgs,
    suggested: Option<String>,
    no_wait: bool,
    timeout_secs: u64,
) -> Result<i32> {
    let ctx = Ctx::new(global)?;
    let IpcResponse::WhoAmI(me) = ctx.ipc(IpcRequest::WhoAmI).await? else {
        bail!("unexpected response to whoami");
    };
    if me.registered {
        println!(
            "you are already enrolled on this mesh as '{}'",
            me.username.as_deref().unwrap_or("(founder)")
        );
        return Ok(0);
    }

    let IpcResponse::EnrolStatus { state, .. } = ctx.ipc(IpcRequest::EnrolStatus).await? else {
        bail!("unexpected response to enrol status");
    };
    let suggested = suggested
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default();
    // A person who re-runs this after closing their terminal should see
    // the *same* code — an admin may already be holding it.
    if state != mesh_proto::EnrolmentState::Pending {
        if suggested.len() > mesh_proto::enrol::MAX_SUGGESTED_USERNAME {
            bail!(
                "--as is too long (max {} characters)",
                mesh_proto::enrol::MAX_SUGGESTED_USERNAME
            );
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).context("system rng unavailable")?;
        let now = mesh_proto::now_micros();
        let mut request = mesh_proto::EnrolmentRequest {
            user_id: ctx.user_id(),
            suggested_username: suggested.clone(),
            from_node: me.node_id,
            nonce,
            created_at_micros: now,
            expires_at_micros: now + mesh_proto::enrol::DEFAULT_TTL_SECS * 1_000_000,
            signature: Vec::new(),
        };
        request.signature = ctx.sign(&request.signing_bytes())?;
        ctx.ipc(IpcRequest::EnrolSubmit {
            request: Box::new(request),
        })
        .await?;
    }

    let IpcResponse::EnrolStatus { code, .. } = ctx.ipc(IpcRequest::EnrolStatus).await? else {
        bail!("unexpected response to enrol status");
    };
    let code = code.unwrap_or_else(|| "(none)".into());
    // The name they asked to be called, on the machine they asked from —
    // the two things an admin will read. The key is the identity, and it
    // is on the next line as the code that encodes it.
    println!(
        "Your identity:  {}@{}",
        if suggested.is_empty() {
            ctx.user_id().short()
        } else {
            suggested.clone()
        },
        me.node_name
    );
    println!("Enrolment code: {code}");
    println!();
    if no_wait {
        println!(
            "{}",
            term::out().prose("Show this code to a mesh admin, then run `meshnode whoami`.")
        );
        return Ok(0);
    }
    println!("Show this code to a mesh admin. Waiting for approval…");

    let now = std::time::Instant::now();
    let deadline = now
        .checked_add(std::time::Duration::from_secs(timeout_secs))
        .unwrap_or_else(|| now + std::time::Duration::from_secs(100 * 365 * 86_400));
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let IpcResponse::EnrolStatus {
            state, declined_by, ..
        } = ctx.ipc(IpcRequest::EnrolStatus).await?
        else {
            bail!("unexpected response to enrol status");
        };
        match state {
            mesh_proto::EnrolmentState::Enrolled => {
                let IpcResponse::WhoAmI(me) = ctx.ipc(IpcRequest::WhoAmI).await? else {
                    bail!("unexpected response to whoami");
                };
                println!();
                println!(
                    "Approved. You are '{}' on this mesh, with {}.",
                    me.username.as_deref().unwrap_or("(unnamed)"),
                    if me.roles.is_empty() {
                        "no roles yet".to_string()
                    } else {
                        format!("role {}", me.roles.join(", "))
                    }
                );
                return Ok(0);
            }
            mesh_proto::EnrolmentState::Declined => {
                println!();
                match declined_by {
                    Some(by) => println!("Declined by {by}."),
                    None => println!("Declined."),
                }
                return Ok(1);
            }
            mesh_proto::EnrolmentState::Expired => {
                println!();
                println!(
                    "{}",
                    term::out().prose(
                        "Nobody acted on the request and it expired. Run `meshnode enrol` again."
                    )
                );
                return Ok(1);
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            println!();
            println!(
                "{}",
                term::out().prose(&format!(
                    "Still waiting after {timeout_secs}s. The request stays in the queue until it \
                 expires — check with `meshnode whoami`."
                ))
            );
            return Ok(1);
        }
    }
}

async fn resolve_user(ctx: &Ctx, arg: &str) -> Result<UserId> {
    if let Ok(id) = UserId::parse(arg) {
        return Ok(id);
    }
    let IpcResponse::Users(rows) = ctx.ipc(IpcRequest::Users).await? else {
        bail!("could not list users to resolve {arg:?}");
    };
    let matches: Vec<UserId> = rows
        .iter()
        .filter(|r| r.username == arg || r.user_id.to_display_string().starts_with(arg))
        .map(|r| r.user_id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!("no user matches {arg:?}"),
        many => bail!("{arg:?} is ambiguous ({} users match)", many.len()),
    }
}

/// Resolve a job argument that may be a full id, a short id, or any unique
/// prefix (git-style) against the mesh's known jobs.
async fn resolve_job(ctx: &Ctx, arg: &str) -> Result<JobId> {
    if let Ok(id) = JobId::parse(arg) {
        return Ok(id);
    }
    // Resolved at the daemon (**H-1**). This used to fetch every job row in
    // the mesh and base32-encode every id here to test one prefix, which
    // made `jobs show` cost more than `jobs ls` — and made it fail outright
    // once the full table stopped fitting in a frame. `limit: 2` is all an
    // ambiguity check needs.
    let resp = ctx
        .ipc(IpcRequest::JobsQuery {
            query: Box::new(mesh_proto::JobsQuery {
                prefix: Some(arg.to_string()),
                limit: 2,
                ..Default::default()
            }),
        })
        .await?;
    let IpcResponse::JobsPage(page) = resp else {
        bail!("could not list jobs to resolve {arg:?}");
    };
    match page.rows.as_slice() {
        [one] => Ok(one.job_id),
        [] => bail!("no job matches {arg:?}"),
        _ => bail!("{arg:?} is ambiguous ({} jobs match)", page.total),
    }
}

/// What a stalled request was waiting for, for the notice and the error
/// (**M-14**). The variant name is enough — this is a hint to a person
/// staring at a quiet terminal, not a log field.
fn request_name(req: &IpcRequest) -> &'static str {
    match req {
        IpcRequest::Status => "status",
        IpcRequest::Jobs | IpcRequest::JobsQuery { .. } => "the job list",
        IpcRequest::JobLogs { .. } => "job logs",
        IpcRequest::Nodes => "the node list",
        IpcRequest::Top | IpcRequest::Totals => "capacity",
        IpcRequest::Run { .. } | IpcRequest::RunScript { .. } => "a submission",
        _ => "this request",
    }
}

fn unexpected(resp: IpcResponse) -> Result<i32> {
    bail!("unexpected daemon response: {resp:?}")
}

// ---------------------------------------------------------------------------
// Resume, halt, results diff (ideas 01–03)
// ---------------------------------------------------------------------------

/// `--halt-on-fail` as the reserved environment key the owner reads.
fn halt_env(spec: Option<&str>) -> Result<Vec<(String, String)>> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };
    let policy = mesh_proto::HaltPolicy::parse(spec).ok_or_else(|| {
        anyhow!("--halt-on-fail takes a count or a percentage (e.g. 3 or 5%), not {spec:?}")
    })?;
    Ok(vec![(
        mesh_proto::parallel::REQ_HALT_ON_FAIL.to_string(),
        policy.to_string(),
    )])
}

/// Fetch the record of an earlier parallel run.
async fn parallel_record(ctx: &Ctx, arg: &str) -> Result<mesh_proto::parallel::ParallelRecord> {
    let job_id = resolve_job(ctx, arg).await?;
    match ctx.ipc(IpcRequest::ParallelRecord { job_id }).await? {
        IpcResponse::ParallelRecord(record) => Ok(*record),
        IpcResponse::Error { message } => bail!("{message}"),
        IpcResponse::Denied { message } => bail!("permission denied: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

/// Build the submission a `--resume` makes: the earlier run's request with
/// only its unfinished items. `None` (after saying so) when there is
/// nothing to do. Returns the items submitted alongside, for `--out`.
async fn resume_request(
    ctx: &Ctx,
    arg: &str,
    capture: Capture,
    halt_on_fail: Option<&str>,
) -> Result<Option<(IpcRequest, Vec<String>)>> {
    let record = parallel_record(ctx, arg).await?;
    if record.capture != capture {
        let (was, verb) = match record.capture {
            Capture::Object => ("a map", "map"),
            Capture::Inline => ("a parallel run", "parallel"),
        };
        bail!(
            "{} was {was}; `meshnode {verb} --resume {}` resumes it",
            record.job.short(),
            record.job.short()
        );
    }
    if record.finished_at_micros.is_none() {
        eprintln!(
            "{}",
            crate::term::err().status(&format!(
                "· {} never recorded an end — its owner may still be running it, or died \
             mid-way; everything without a success is being re-run",
                record.job.short()
            ))
        );
    }
    let items = record.unfinished_items();
    let (failed, missing) = (record.failed(), record.missing());
    if items.is_empty() {
        println!(
            "nothing to resume: every one of {} item(s) of {} succeeded",
            record.items.len(),
            record.job.short()
        );
        return Ok(None);
    }
    eprintln!(
        "↻ resuming {} — {} of {} item(s) unfinished ({failed} failed, {missing} never ran)",
        record.job.short(),
        items.len(),
        record.items.len()
    );
    let mut env = record.env.clone();
    env.retain(|(k, _)| {
        k != mesh_proto::parallel::REQ_RESUMES && k != mesh_proto::parallel::REQ_HALT_ON_FAIL
    });
    env.push((
        mesh_proto::parallel::REQ_RESUMES.to_string(),
        record.job.to_display_string(),
    ));
    env.extend(halt_env(halt_on_fail)?);
    let inner = IpcRequest::Parallel {
        command: record.command,
        service: record.service,
        inputs: record.inputs,
        item_input: record.item_input,
        items: items.clone(),
        on: record.on,
        env,
        timeout_secs: record.timeout_secs,
        chunk: record.chunk,
        capture,
        reduce: record.reduce,
        reduce_on: record.reduce_on,
    };
    Ok(Some((inner, items)))
}

/// A run, keyed the way its diff compares it.
enum RunSide {
    Parallel(Box<mesh_proto::parallel::ParallelRecord>),
    Fanout(Box<mesh_proto::ResultSet>),
}

impl RunSide {
    fn label(&self) -> String {
        match self {
            RunSide::Parallel(r) => format!("{} ({})", r.job.short(), r.what()),
            RunSide::Fanout(s) => format!("{} ({})", s.group.short(), s.preview),
        }
    }

    fn keyed(&self) -> Vec<(String, diff::Side)> {
        match self {
            RunSide::Parallel(r) => r
                .outcomes
                .iter()
                .map(|o| {
                    (
                        o.item.clone(),
                        diff::Side {
                            ok: o.succeeded(),
                            exit_code: o.exit_code,
                            fingerprint: Some(match &o.output {
                                Some(id) => id.to_display_string(),
                                None => diff::hex(&o.stdout_digest),
                            }),
                            node: o.node_name.clone(),
                        },
                    )
                })
                .collect(),
            RunSide::Fanout(s) => s
                .rows
                .iter()
                .map(|r| {
                    (
                        r.node_name.clone(),
                        diff::Side {
                            ok: r.succeeded(),
                            exit_code: r.exit_code,
                            fingerprint: r
                                .output(mesh_core::outputs::OUT_STDOUT)
                                .map(|out| diff::hex(&mesh_proto::parallel::stdout_digest(out))),
                            node: r.node_name.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// Resolve a `results diff` argument: a fan-out group, or a parallel job.
async fn run_side(ctx: &Ctx, arg: &str) -> Result<RunSide> {
    if let Ok(group) = resolve_fanout(ctx, arg).await {
        let resp = ctx
            .ipc(IpcRequest::Results {
                group,
                failed_only: false,
            })
            .await?;
        if let IpcResponse::Results(set) = resp {
            return Ok(RunSide::Fanout(set));
        }
    }
    match parallel_record(ctx, arg).await {
        Ok(record) => Ok(RunSide::Parallel(Box::new(record))),
        Err(e) => {
            bail!("{arg:?} is neither a fan-out group held here nor a parallel run owned here: {e}")
        }
    }
}

/// One fan-out group's rows, printed.
///
/// Shared by `meshnode results <group>` and `meshnode history results
/// <group>`: the second is the spelling that stays, and two code paths for
/// one table is how they come to disagree about the exit status.
async fn print_fanout_results(ctx: &Ctx, group: &str, failed: bool, output: Output) -> Result<i32> {
    let group = resolve_fanout(ctx, group).await?;
    let resp = ctx
        .ipc(IpcRequest::Results {
            group,
            failed_only: failed,
        })
        .await?;
    let IpcResponse::Results(set) = resp else {
        return unexpected(resp);
    };
    format::print_result_set(&set, output == Output::Json)?;
    if output != Output::Json {
        format::print_deep_link(format::DeepLinkKind::Fanout, &group.to_display_string());
    }
    // A set with a failed row exits non-zero, so this reads as a check in a
    // script the way `--expect` does live.
    Ok(if set.failed() > 0 { 1 } else { 0 })
}

/// `meshnode results diff BEFORE AFTER`.
async fn results_diff(ctx: &Ctx, before: &str, after: &str, all: bool, json: bool) -> Result<i32> {
    let (x, y) = (run_side(ctx, before).await?, run_side(ctx, after).await?);
    if matches!(
        (&x, &y),
        (RunSide::Parallel(_), RunSide::Fanout(_)) | (RunSide::Fanout(_), RunSide::Parallel(_))
    ) {
        bail!(
            "one of these is a fan-out (compared by node) and the other a parallel run \
             (compared by item); diff two of the same kind"
        );
    }
    let (rows, summary) = diff::compare(&x.keyed(), &y.keyed());
    if json {
        let shown: Vec<&diff::Row> = rows
            .iter()
            .filter(|r| all || r.change != diff::Change::Same)
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "before": x.label(),
                "after": y.label(),
                "summary": summary,
                "rows": shown,
            }))?
        );
    } else {
        format::print_run_diff(&x.label(), &y.label(), &rows, &summary, all);
    }
    // Non-zero when anything got worse, so this reads as a check in a
    // script: "did Friday's run regress against Monday's".
    Ok(i32::from(summary.now_failing > 0 || summary.changed > 0))
}

// ---------------------------------------------------------------------------
// `run --fallback local` (idea 07)
// ---------------------------------------------------------------------------

/// The daemon's own words for "nobody can take this": the one refusal a
/// local fallback is allowed to answer.
fn is_no_capacity(message: &str) -> bool {
    message.starts_with("no live node") || message.contains("matches no live node")
}

/// A connection failure to the local socket, as `ipc::connect` words it.
fn is_no_daemon(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<mesh_core::CoreError>(),
        Some(mesh_core::CoreError::Ipc(m)) if m.starts_with("cannot reach daemon")
    )
}

/// Run the command here, verbatim, the way the mesh would have: `bash -c`,
/// the user's environment on top of this shell's, stdio inherited. Said
/// out loud first, because a fallback nobody noticed is a job that
/// silently ran on the wrong machine.
async fn run_locally(
    command: &str,
    env: &[(String, String)],
    timeout_secs: Option<u64>,
    why: &str,
) -> Result<i32> {
    eprintln!(
        "{}",
        crate::term::err().status(&format!("→ running here (fallback: {why})"))
    );
    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("MESHWARE_FALLBACK", "local")
        .spawn()
        .context("cannot start bash for the local fallback")?;
    let status = match timeout_secs {
        Some(secs) => {
            match tokio::time::timeout(std::time::Duration::from_secs(secs), child.wait()).await {
                Ok(status) => status?,
                Err(_) => {
                    let _ = child.kill().await;
                    eprintln!(
                        "{}",
                        crate::term::err()
                            .status(&format!("← timed out after {secs}s (local fallback)"))
                    );
                    return Ok(124);
                }
            }
        }
        None => child.wait().await?,
    };
    let code = status.code().unwrap_or(1);
    eprintln!(
        "{}",
        crate::term::err().status(&format!(
            "← {} (local fallback)",
            if code == 0 {
                "succeeded".to_string()
            } else {
                format!("your command exited {code}")
            }
        ))
    );
    Ok(code)
}

// ---------------------------------------------------------------------------
// `jobs wait` (idea 24)
// ---------------------------------------------------------------------------

async fn jobs_wait(
    ctx: &Ctx,
    job_id: JobId,
    then: Option<&str>,
    timeout: Option<&str>,
    notify: bool,
) -> Result<i32> {
    let deadline = match timeout {
        Some(text) => Some(
            std::time::Instant::now()
                + humantime::parse_duration(text)
                    .with_context(|| format!("{text:?} is not a duration (try 30m, 2h)"))?,
        ),
        None => None,
    };
    let started = std::time::Instant::now();
    let mut announced = false;
    let row = loop {
        let resp = ctx.ipc(IpcRequest::JobShow { job_id }).await?;
        let IpcResponse::JobShow(row) = resp else {
            return unexpected(resp);
        };
        if row.state.is_terminal() {
            break row;
        }
        if !announced {
            eprintln!(
                "{}",
                crate::term::err().status(&format!(
                    "· waiting for {} ({}) on {}…",
                    job_id.short(),
                    row.state.as_str(),
                    row.executor_name.as_deref().unwrap_or("?")
                ))
            );
            announced = true;
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            eprintln!(
                "{}",
                crate::term::err().status(&format!(
                    "← still {} after {timeout:?}; gave up waiting",
                    row.state.as_str()
                ))
            );
            return Ok(124);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    };
    let code = row
        .exit_code
        .unwrap_or(i32::from(row.state != mesh_proto::JobState::Succeeded));
    eprintln!(
        "{}",
        crate::term::err().status(&format!(
            "← {}",
            format::job_outcome(row.state, row.exit_code)
        ))
    );
    let node = row
        .executor_name
        .clone()
        .unwrap_or_else(|| "the mesh".into());
    let seconds = row
        .finished_at_micros
        .map(|f| f.saturating_sub(row.created_at_micros) / 1_000_000)
        .unwrap_or_else(|| started.elapsed().as_secs());
    if notify {
        let event = notify::job_event(
            &job_id.short(),
            &node,
            &row.cmd_preview,
            row.state,
            row.exit_code,
            seconds,
        );
        for note in notify::send(&ctx.config.notify, &event).await {
            eprintln!("{}", crate::term::err().status(&format!("· {note}")));
        }
    }
    if let Some(then) = then {
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(then)
            // The passphrase a person exported for this command is not the
            // hook's to read (SEC-23).
            .env_remove(mesh_core::keyfile::PASSPHRASE_ENV)
            .env("MESHWARE_JOB", job_id.to_display_string())
            .env("MESHWARE_STATE", row.state.as_str())
            .env("MESHWARE_EXIT", code.to_string())
            .env("MESHWARE_NODE", &node)
            .status()
            .await
            .context("cannot run the --then command")?;
        if !status.success() {
            eprintln!(
                "{}",
                crate::term::err().status(&format!(
                    "← --then command exited {}",
                    status.code().unwrap_or(-1)
                ))
            );
            return Ok(status.code().unwrap_or(1));
        }
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// `top --follow` / `--devices` (ideas 10, 23)
// ---------------------------------------------------------------------------

async fn top(ctx: &Ctx, json: bool, follow: bool, interval: u64, devices: bool) -> Result<i32> {
    let mut history = spark::History::default();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
    loop {
        ticker.tick().await;
        let frame = if devices {
            let resp = ctx.ipc(IpcRequest::Devices { node: None }).await?;
            let IpcResponse::Devices(rows) = resp else {
                return unexpected(resp);
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(0);
            }
            if follow {
                for r in &rows {
                    history.push(&format!("{}:{}", r.node_name, r.index), r.util_pct);
                }
                history.tick();
            }
            format::render_devices_live(&rows, follow.then_some(&history))
        } else {
            let resp = ctx.ipc(IpcRequest::Top).await?;
            let IpcResponse::Top(rows) = resp else {
                return unexpected(resp);
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(0);
            }
            if follow {
                for r in &rows {
                    history.push(
                        &r.node_id.to_display_string(),
                        r.system.as_ref().and_then(|s| s.cpu_pct),
                    );
                }
                history.tick();
            }
            format::render_top(&rows, follow.then_some(&history))
        };
        if !follow {
            print!("{frame}");
            return Ok(0);
        }
        // Clear and home, then the frame, then the footer that says what
        // the sparkline spans. A full-screen alternate buffer would be
        // tidier and would take the last frame with it on exit; leaving the
        // final table in the scrollback is the point of a `top`.
        let span = history.samples() as u64 * interval;
        print!(
            "\x1b[2J\x1b[H{frame}\n  every {interval}s · sparkline spans the last {} · Ctrl-C to stop\n",
            humantime::format_duration(std::time::Duration::from_secs(span))
        );
        std::io::stdout().flush()?;
    }
}

// ---------------------------------------------------------------------------
// `drain --until` / `--for` / `--lift` (idea 20)
// ---------------------------------------------------------------------------

async fn drain_timed(
    ctx: &Ctx,
    until: Option<&str>,
    for_: Option<&str>,
    reason: Option<String>,
    lift: bool,
    move_pending: bool,
) -> Result<i32> {
    let until_micros = if lift {
        None
    } else if let Some(hhmm) = until {
        let (h, m) = hhmm
            .split_once(':')
            .and_then(|(h, m)| Some((h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?)))
            .filter(|(h, m)| *h < 24 && *m < 60)
            .ok_or_else(|| anyhow!("--until takes a local time as HH:MM, not {hhmm:?}"))?;
        Some(mesh_core::localtime::next_local_hhmm(h, m))
    } else if let Some(text) = for_ {
        let d = humantime::parse_duration(text)
            .with_context(|| format!("--for takes a duration (2h, 45m), not {text:?}"))?;
        if d.as_secs() == 0 {
            bail!("--for 0 is not a drain");
        }
        Some(mesh_proto::now_micros() + u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    } else {
        unreachable!("the caller checked one of --until, --for, --lift")
    };
    let resp = ctx
        .ipc(IpcRequest::DrainTimed {
            until_micros,
            reason: reason.unwrap_or_default(),
            move_pending,
        })
        .await?;
    match resp {
        IpcResponse::Notice { message } => {
            println!("{message}");
            Ok(0)
        }
        IpcResponse::Denied { message } => bail!("permission denied: {message}"),
        IpcResponse::Error { message } => bail!("{message}"),
        other => unexpected(other),
    }
}

// ---------------------------------------------------------------------------
// `nodes diff` (idea 25)
// ---------------------------------------------------------------------------

async fn node_facts(
    ctx: &Ctx,
    arg: &str,
    collector: Option<&str>,
) -> Result<(String, Vec<(String, String)>)> {
    let resp = ctx
        .ipc(IpcRequest::Facts {
            subject: arg.to_string(),
            collector: collector.map(String::from),
        })
        .await?;
    let rows = match resp {
        IpcResponse::Facts(rows) => rows,
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    };
    let name = rows
        .first()
        .map(|r| r.subject_name.clone())
        .unwrap_or_else(|| arg.to_string());
    let mut flat = Vec::new();
    for row in &rows {
        let doc = mesh_core::facts::parse(&row.doc);
        for (k, v) in mesh_core::facts::flatten(&doc) {
            flat.push((format!("{}.{k}", row.collector), v));
        }
    }
    Ok((name, flat))
}

async fn nodes_diff(
    ctx: &Ctx,
    left: &str,
    right: &str,
    collector: Option<&str>,
    all: bool,
    json: bool,
) -> Result<i32> {
    let (l_name, l) = node_facts(ctx, left, collector).await?;
    let (r_name, r) = node_facts(ctx, right, collector).await?;
    if l.is_empty() || r.is_empty() {
        let who = if l.is_empty() { &l_name } else { &r_name };
        bail!(
            "{who} has published no facts yet — the built-in collector runs on every node once \
             it is up; `meshnode facts {who}` shows what has arrived"
        );
    }
    let (mut rows, same) = diff::compare_facts(&l, &r);
    // Likely culprits first, then everything else by key.
    rows.sort_by(|a, b| {
        diff::is_likely_culprit(&b.key)
            .cmp(&diff::is_likely_culprit(&a.key))
            .then_with(|| a.key.cmp(&b.key))
    });
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "left": l_name,
                "right": r_name,
                "differ": rows,
                "same": same,
            }))?
        );
        return Ok(i32::from(!rows.is_empty()));
    }
    let agree: Vec<(String, String)> = if all {
        l.iter()
            .filter(|(k, v)| r.iter().any(|(k2, v2)| k2 == k && v2 == v))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    format::print_nodes_diff(&l_name, &r_name, &rows, same, &agree);
    Ok(i32::from(!rows.is_empty()))
}

/// Does a bootstrap entry look reachable from outside this LAN? Hostnames
/// are trusted (the operator chose them); literal IPs must be globally
/// routable.
fn is_public_bootstrap(addr: &str) -> bool {
    match addr.parse::<std::net::SocketAddr>() {
        Ok(sa) => mesh_core::wan::is_public_ip(sa.ip()),
        Err(_) => addr
            .rsplit_once(':')
            .map(|(host, _)| {
                host.parse::<std::net::IpAddr>()
                    .map(mesh_core::wan::is_public_ip)
                    .unwrap_or(true)
            })
            .unwrap_or(true),
    }
}

/// The exit code a set of checks implies: 1 if anything failed.
///
/// Split out of the renderer so `--output json` reports the same code as
/// the table does. A JSON health check that always exits 0 would be worse
/// than no JSON at all — an agent would read "healthy" off a failure (§7).
fn worst_verdict(checks: &[mesh_core::doctor::Check]) -> i32 {
    i32::from(
        checks
            .iter()
            .any(|c| c.verdict == mesh_core::doctor::Verdict::Fail),
    )
}

/// Render `meshnode doctor` checks as a table; returns the process exit code
/// (1 if anything failed).
fn print_doctor(checks: &[mesh_core::doctor::Check]) -> i32 {
    use mesh_core::doctor::Verdict;
    let ink = term::out();
    let mut code = 0;
    for c in checks {
        // Wide enough for the longest name, which is `runtime container`
        // since T4. A name that overflows its column pushes every detail
        // on that row out of alignment, and a `doctor` that does not line
        // up is one people skim. Padded before it is painted: the colour
        // has no width on screen, and `{:<18}` would count it.
        let name = format!("{:<18}", c.name);
        // A passing check recedes, so the ones that did not are what a
        // skimming eye finds.
        let (mark, name, detail) = match c.verdict {
            Verdict::Ok => (ink.good("ok  "), name, ink.dim(&c.detail)),
            Verdict::Warn => (ink.warn("warn"), ink.bold(name), c.detail.clone()),
            Verdict::Fail => {
                code = 1;
                (ink.sgr("1;31", "FAIL"), ink.bold(name), c.detail.clone())
            }
        };
        println!("{mark}  {name}  {detail}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_the_way_operators_write_them() {
        for (input, expected) in [
            ("512M", 512),
            ("512MB", 512),
            ("48G", 48 * 1024),
            ("48g", 48 * 1024),
            ("48GB", 48 * 1024),
            ("2T", 2 * 1024 * 1024),
            ("1.5G", 1536),
            ("1024K", 1),
            ("4096", 4096), // bare = MB
            (" 8G ", 8 * 1024),
        ] {
            assert_eq!(
                parse_size_mb("--mem", input).expect(input),
                expected,
                "parsing {input}"
            );
        }
        for bad in ["", "lots", "-4G", "G", "4X"] {
            assert!(parse_size_mb("--mem", bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn requirements_become_reserved_env() {
        let reqs = parse_requirements(Some(4), Some("48G"), None).expect("parse");
        assert_eq!(reqs.cpus, 4);
        assert_eq!(reqs.mem_mb, 48 * 1024);
        assert_eq!(reqs.disk_mb, 0);
        assert_eq!(
            mesh_proto::Requirements::from_env(&reqs.to_env()),
            reqs,
            "what the CLI sends is what the executor reads"
        );
        // Nothing declared stays nothing: no env entries at all.
        assert!(
            parse_requirements(None, None, None)
                .expect("parse")
                .to_env()
                .is_empty()
        );
    }

    #[test]
    fn reserved_env_keys_are_refused_with_a_pointer_to_the_flags() {
        let err = parse_env(&["MESHWARE_TASK_CPUS=99".to_string()]).expect_err("refused");
        assert!(err.to_string().contains("--cpus"), "unhelpful: {err}");
        // Ordinary env is untouched.
        assert_eq!(
            parse_env(&["FOO=bar".to_string()]).expect("ok"),
            vec![("FOO".to_string(), "bar".to_string())]
        );
    }
}

/// L1 P1 — the eight-verb greeting.
///
/// These assert the *promise*, not the implementation: the greeting is the
/// published mental model, and hiding a verb from it never takes anything
/// away (D-L1.3). The second test is the load-bearing one — a future
/// `#[command(hide)]` would silently break it, because `clap_complete` and
/// `clap_mangen` drop hidden subcommands without a word.
#[cfg(test)]
mod greeting {
    use super::*;

    /// Assembling clap's tree is recursive and this one is wide: 60
    /// top-level verbs, 194 pages under them. In a debug build the frames
    /// add up to more than the 2 MiB a test thread is given, so anything
    /// that touches `Cli::command()` runs somewhere with room. The shipped
    /// binary parses on the main thread's 8 MiB and has never come near it.
    pub(super) fn on_a_real_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(f)
            .expect("spawning the help-tree thread")
            .join()
            .expect("building the help tree")
    }

    /// clap's own consistency checks, over every verb. Two arguments that
    /// answered to `--cosign` shipped because nothing asked: a debug build
    /// panicked on `upgrade --help`, and a release build parsed the
    /// daemon's own fleet-rollout command line as a conflict.
    ///
    /// Each verb is asserted **as the command being run**, not only as a
    /// child of the root: clap checks an argument's `conflicts_with` when it
    /// builds the command that owns it, and a reference to a removed
    /// argument inside `parallel` got past the root-only assertion and
    /// panicked on `meshnode parallel` instead.
    #[test]
    fn every_verb_passes_clap_s_own_checks() {
        on_a_real_stack(|| {
            let root = full_command();
            for verb in root.get_subcommands() {
                verb.clone().debug_assert();
            }
            root.debug_assert();
        });
    }

    #[test]
    fn the_greeting_is_the_masterplan_verbs_in_the_masterplan_order() {
        // Sorted by display order, which is what the help printer reads.
        let shown = on_a_real_stack(|| {
            let mut shown: Vec<(usize, String)> = cli_command()
                .get_subcommands()
                .filter(|c| !c.is_hide_set())
                .map(|c| (c.get_display_order(), c.get_name().to_string()))
                .collect();
            shown.sort();
            shown.into_iter().map(|(_, n)| n).collect::<Vec<_>>()
        });
        assert_eq!(shown, GREETED, "the greeting drifted from MASTERPLAN §1");
    }

    #[test]
    fn every_hidden_verb_still_parses_and_still_has_a_page() {
        // Nothing was removed to make the list of eight: the tree that
        // `help --all`, `completion` and `manpage` read has every verb the
        // parser has, and none of them hidden.
        let (all, full, parsed) = on_a_real_stack(|| {
            let mut all: Vec<String> = cli_command()
                .get_subcommands()
                .map(|c| c.get_name().to_string())
                .collect();
            let mut full: Vec<String> = full_command()
                .get_subcommands()
                .filter(|c| !c.is_hide_set())
                .map(|c| c.get_name().to_string())
                .collect();
            all.sort();
            full.sort();
            // And a hidden verb parses exactly as it did before it was hidden.
            // (`deployment`, since `workflow` was removed on 2026-09-12.)
            let parsed = cli_command()
                .try_get_matches_from(["meshnode", "deployment", "ls"])
                .expect("a hidden verb still parses")
                .subcommand_name()
                .map(str::to_string);
            (all, full, parsed)
        });
        assert_eq!(
            full, all,
            "`help --all`, completions and man pages must see every verb"
        );
        assert!(
            all.len() > GREETED.len() + 40,
            "expected the greeting to be hiding most of the CLI, got {}",
            all.len()
        );
        assert_eq!(parsed.as_deref(), Some("deployment"));
    }

    /// `upgrade --rollback`, `adopt`'s remote probe and the build scripts
    /// read the version off the first line; everything `--version` added
    /// in DOC-14 comes after it, and `-V` is that line alone.
    #[test]
    fn the_first_line_of_version_is_still_just_the_version() {
        on_a_real_stack(|| {
            let cmd = cli_command();
            let want = format!("meshnode {}", env!("CARGO_PKG_VERSION"));
            let long = cmd.render_long_version();
            assert_eq!(long.lines().next(), Some(want.as_str()), "{long}");
            assert!(long.contains("\nprotocol 2."), "{long}");
            assert_eq!(cmd.render_version().trim(), want);
        });
    }

    #[test]
    fn the_footer_counts_what_it_is_not_showing() {
        let hidden = on_a_real_stack(|| {
            cli_command()
                .get_subcommands()
                .filter(|c| c.is_hide_set())
                .count()
        });
        let footer = greeting_footer(hidden);
        assert!(footer.contains("meshnode help --all"));
        // The dim escape is styling, never content: strip it and the
        // sentence still reads — which is what a pipe or a log sees.
        assert!(
            footer
                .replace(DIM, "")
                .replace(UNDIM, "")
                .ends_with(&format!("{hidden} more commands — `meshnode help --all`")),
            "the count must be the real one: {footer:?}"
        );
    }

    #[test]
    fn pair_is_an_alias_over_the_paths_that_already_worked() {
        let outcomes = on_a_real_stack(|| {
            [
                vec!["meshnode", "pair"],
                vec!["meshnode", "pair", "--join"],
                vec!["meshnode", "pair", "--approve"],
                vec!["meshnode", "pair", "--addr", "10.0.0.9:7777"],
                // A machine is on one side of the handshake or the other.
                vec!["meshnode", "pair", "--join", "--approve"],
                // The paths `pair` fronts are untouched.
                vec!["meshnode", "join", "--pair"],
                vec!["meshnode", "invite", "approve"],
            ]
            .map(|argv| cli_command().try_get_matches_from(argv).is_ok())
        });
        assert_eq!(
            outcomes,
            [true, true, true, true, false, true, true],
            "`pair` must front both sides, and only one at a time"
        );
    }
}

#[cfg(test)]
mod help_groups {
    use super::*;

    /// Every verb the parser declares.
    ///
    /// Through `greeting::on_a_real_stack`, which already exists for the
    /// same reason: `Cli::command()` recurses deeply enough to overflow a
    /// test thread's default 2 MiB.
    fn subcommand_names() -> Vec<String> {
        super::greeting::on_a_real_stack(|| {
            Cli::command()
                .get_subcommands()
                .map(|s| s.get_name().to_string())
                .collect::<Vec<_>>()
        })
    }

    /// Every verb `help --all` can show is filed under a heading.
    ///
    /// **The guard on the listing, not on the grouping.** `HELP_GROUPS` is
    /// a hand-written table and a new verb does not add itself to one, so
    /// without this the first command somebody adds after today lands in a
    /// nameless `Commands:` block at the bottom and nobody notices. The
    /// failure names the verb, so filing it is a one-line fix rather than
    /// an investigation.
    #[test]
    fn every_verb_is_filed_under_a_heading() {
        let filed: Vec<&str> = HELP_GROUPS
            .iter()
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        let unfiled: Vec<String> = subcommand_names()
            .into_iter()
            .filter(|n| !filed.contains(&n.as_str()))
            .collect();
        assert!(
            unfiled.is_empty(),
            "these verbs have no heading in `meshnode help --all`: {unfiled:?} — \
             add each to HELP_GROUPS"
        );
    }

    /// And the table names no verb that does not exist.
    ///
    /// The other direction, and the one a *removal* breaks: deleting a verb
    /// without deleting its row leaves a heading promising something that
    /// is gone.
    #[test]
    fn the_table_names_no_verb_that_is_gone() {
        let real = subcommand_names();
        let ghosts: Vec<&str> = HELP_GROUPS
            .iter()
            .flat_map(|(_, v)| v.iter().copied())
            .filter(|v| !real.iter().any(|r| r == v))
            .collect();
        assert!(
            ghosts.is_empty(),
            "HELP_GROUPS names verbs that no longer exist: {ghosts:?}"
        );
    }
}
