//! Every `meshnode <verb> <subcommand>` list, as clap sees it.
//!
//! Split out of `main.rs` on 2026-09-20 (CUT-2). That file was 15,188 lines
//! and about a third of it was this: thirty-eight enums that say what the
//! command line *accepts*, interleaved with the code that acts on it, so
//! reading either meant scrolling past the other.
//!
//! They are data, not behaviour — which is what makes this a seam rather
//! than a cut. `Command` itself stays in `main.rs`, because its variants
//! carry the top-level flags and it is what `run` matches on first.

use clap::{Subcommand, ValueEnum};

use crate::*;

#[derive(Clone, Copy, ValueEnum, Debug, PartialEq, Eq)]
pub(crate) enum Output {
    Table,
    Json,
}

#[derive(Subcommand)]
pub(crate) enum StateCmd {
    /// Op-log size, what a pass would drop, and what the horizon is holding
    Show {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Compact this node's own authored ops now (admin)
    Compact {
        /// Override `[state] compaction_horizon` for this pass, e.g. 7d
        #[arg(long, value_name = "DURATION")]
        horizon: Option<String>,
        /// Report what would be dropped without dropping anything
        #[arg(long)]
        dry_run: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What the op-log is made of, by entity, and how fast it grows
    ///
    /// Separate from `show` because it walks the *whole* log rather than
    /// this node's own ops: it is the measurement retention windows are
    /// sized against, and `state compact` should not start paying for it.
    Census {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// What this node is refusing, and what is churning
    ///
    /// Empty on a healthy mesh. It fills in when one entity starts
    /// authoring ops far faster than the mesh will carry — a looping
    /// workflow, a flapping collector — and names the offender, because a
    /// limit that does not say what tripped is a bug report nobody can act
    /// on.
    Pressure {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Rebuild this node's op-log from a member, when its own will not open
    ///
    /// Offline: stop the daemon first. The node keeps its identity; the
    /// broken op-log is moved aside, not deleted. Everything is fetched
    /// from the named member, and the node writes nothing of its own on
    /// its next start until it has synced with a peer.
    Rebuild {
        /// A member of this mesh to fetch from, as host:port (repeatable)
        #[arg(long, value_name = "HOST:PORT", required = true)]
        from: Vec<String>,
        /// Required: the current op-log is moved aside
        #[arg(long)]
        yes: bool,
    },
}

/// `Add` carries every field of a sink, so it dwarfs its siblings. Boxed
/// or not, clap builds exactly one of these per process — the same trade
/// `FactsCmd` above declines, for the same reason.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub(crate) enum SinksCmd {
    /// What every sink is doing: lag, sent, spooled, dropped, last status
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Create or replace a sink (admin)
    Add {
        /// A name for the sink
        name: String,
        /// plugin | clickhouse | victorialogs | webhook
        ///
        /// Required, with no default: where a mesh's history goes is not a
        /// thing to pick on the operator's behalf. `plugin` is the one that
        /// needs nothing else running — your program gets the rows as
        /// NDJSON on stdin and writes them wherever you want.
        #[arg(long)]
        kind: String,
        /// Destination URL, for the kinds that have one
        #[arg(long)]
        url: Option<String>,
        /// The executable, for `--kind plugin`
        #[arg(long, value_name = "PATH")]
        plugin: Option<String>,
        /// The database rows go into (ClickHouse), or the index prefix
        /// (OpenSearch: `<prefix>-jobs`, default `meshware`). Without it,
        /// ClickHouse uses the connection's default database
        #[arg(long, value_name = "NAME")]
        database: Option<String>,
        /// Retired, and accepted only so an older script fails with a
        /// sentence instead of "unexpected argument"
        #[arg(long, value_name = "PATH", hide = true)]
        file: Option<String>,
        /// A `{{secret:name}}` reference. Never a literal — the record is
        /// replicated to every node and never compacted away.
        #[arg(long, value_name = "REF")]
        credential: Option<String>,
        /// A second `{{secret:name}}` reference, for reading job output
        /// back
        ///
        /// Separate from --credential because a token that may INSERT
        /// should not have to SELECT. Without one, replay is refused and
        /// says so rather than quietly reusing the write token.
        #[arg(long, value_name = "REF")]
        read_credential: Option<String>,
        /// Which tables to export: `jobs`, `audit`, `usage`
        #[arg(long, default_value = "jobs")]
        tables: String,
        /// Only export from nodes matching this selector, e.g. `tag=sink`.
        /// The control that matters when only some machines have a route.
        #[arg(long, value_name = "SELECTOR")]
        run_on: Option<String>,
        /// Rows per batch
        #[arg(long)]
        batch: Option<u32>,
        /// Spool cap in MB before the oldest batches are dropped
        #[arg(long, value_name = "MB")]
        spool_mb: Option<u64>,
        /// Also export raw job stdout/stderr from the node that produced it
        ///
        /// Off by default everywhere except VictoriaLogs, which is a log
        /// store and is what raw output is for. Think before turning it on
        /// for a destination priced per gigabyte.
        #[arg(long)]
        payloads: bool,
        /// Refuse to export output larger than this many MB (0 = no cap).
        /// Above it the record carries the reference and the bytes stay put.
        #[arg(long, value_name = "MB")]
        payload_max_mb: Option<u64>,
        /// Hold deletion back this many days while this sink is behind
        ///
        /// *"Don't delete until it is exported."* Capped at 30 days: past
        /// the ceiling the mesh keeps deleting and reports the export gap
        /// as permanent, because a stalled sink must never pin the op-log
        /// open on every node for ever. 0 (the default) is no hold.
        #[arg(long, value_name = "DAYS")]
        hold_for_export_days: Option<u32>,
        /// Create it switched off
        #[arg(long)]
        disabled: bool,
        /// What the sink is for, shown in `meshnode sinks`
        #[arg(long)]
        description: Option<String>,
        /// Check it and print it, without publishing
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the DDL for a sink's destination — **never runs it**
    ///
    /// meshware stays out of the migration business: your warehouse is
    /// yours, and this tells you exactly what shape we will send so you can
    /// apply it on your own schedule.
    Ddl {
        /// The sink's name
        name: String,
        /// clickhouse. Defaults to the sink's kind.
        #[arg(long)]
        dialect: Option<String>,
    },
    /// Send one synthetic row and print the response verbatim (admin)
    ///
    /// An operator should learn about a wrong token here, not from a lag
    /// graph two hours later.
    Test { name: String },
    /// Retire a sink (admin). Rows already exported are untouched.
    Rm { name: String },
    /// Start exporting again after `meshnode restore` held every sink
    /// (admin)
    Resume,
}

#[derive(Subcommand)]
pub(crate) enum RetentionCmd {
    /// The current windows, who signed them, and where the log settles
    Show {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Set one or more windows (admin): `runs=30d facts=1y audit=forever`
    Set {
        /// `<table>=<window>` pairs. Tables: runs, facts, job-output,
        /// audit, archive, content. Windows: `90d`, `12w`, `1y`, `forever`.
        #[arg(value_name = "TABLE=WINDOW", required = true)]
        windows: Vec<String>,
        /// Also set a floor nobody may go below: `--floor runs=7d`
        #[arg(long, value_name = "TABLE=WINDOW")]
        floor: Vec<String>,
        /// Show what this would settle the log at, and sign nothing
        #[arg(long)]
        dry_run: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Withdraw the policy, returning the mesh to the shipped defaults
    /// (admin)
    Reset,
}

#[derive(Subcommand)]
pub(crate) enum SupportCmd {
    /// Write the bundle
    Bundle {
        /// Where to write it (default: ./meshware-support-<unix-time>.json)
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
        /// How far back to take the audit tail (e.g. 24h, 7d)
        #[arg(long, value_name = "DURATION", default_value = "24h")]
        since: String,
        /// Most audit entries to include
        #[arg(long, default_value = "200")]
        limit: u32,
    },
}

#[derive(Subcommand)]
pub(crate) enum IdentityCmd {
    Show {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Put a passphrase on this account's user key
    ///
    /// The key is 32 bytes and it is your whole identity — with the `admin`
    /// role it can grant itself anything. Mode 0600 stops the other account
    /// on this machine and nothing else: not a backup, not a sync client,
    /// not a stolen disk. This seals it with XChaCha20-Poly1305 under a key
    /// derived from your passphrase with Argon2id.
    ///
    /// You then unlock once per login session. It does *not* protect the key
    /// from something running as you while it is unlocked — that is hardware
    /// binding, which is a different feature. See "Keys at rest" in
    /// docs/operating/limits.md.
    Protect,
    /// Take the passphrase back off, leaving a plaintext key
    Unprotect {
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Open the protected key for this login session
    ///
    /// The unlock is kept in $XDG_RUNTIME_DIR — tmpfs, mode 0600, cleared
    /// when you log out — and expires after twelve hours. What is kept is
    /// the key-encryption key, never the signing key itself.
    Unlock,
    /// Forget the unlock now, without waiting for it to expire
    Lock,
    /// Whether this key is protected, and whether it is unlocked
    Status {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Keep this account's key on a token instead of on this disk
    ///
    /// `identity protect` seals the key at rest; this removes it. The
    /// private half is generated on the device and never comes out, so
    /// there is nothing on this machine to steal, seal or back up — a
    /// process running as you while the key is "unlocked" can ask for
    /// signatures while the token is plugged in, and cannot walk away with
    /// the identity.
    ///
    /// meshnode never talks to the hardware itself. It
    /// asks an agent that already does: `gpg-agent --enable-ssh-support`
    /// for an OpenPGP card, or `ssh-add -s /usr/lib64/opensc-pkcs11.so`
    /// (or libykcs11.so) for an Ed25519 key in PIV slot 9a.
    Hardware {
        #[command(subcommand)]
        cmd: HardwareCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum HardwareCmd {
    /// Ed25519 keys the agent is holding right now
    Ls {
        /// The agent socket. Defaults to $MESHWARE_AGENT_SOCK, then
        /// $SSH_AUTH_SOCK.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Point this account's identity file at a key the agent holds
    ///
    /// **This does not move your existing key onto a token** — nothing can.
    /// A non-exportable key is generated on the device, so it is a
    /// *different* key, which means a different identity. A key the mesh
    /// does not know yet is refused, with the steps that let it in: `users
    /// rotate`, or for the founder, whose key can never be rotated, a
    /// second admin. A key the mesh still honours is moved aside, never
    /// overwritten. A key that is one of the files in ~/.ssh is refused:
    /// binding to it would protect nothing.
    Use {
        /// A `mwu1…` id, or a prefix of one, as `hardware ls` prints it
        #[arg(value_name = "KEY")]
        key: String,
        /// The ssh-agent socket that holds the key (default: $MESHWARE_AGENT_SOCK, then $SSH_AUTH_SOCK)
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Bind even though the key is a file in ~/.ssh, the mesh does not
        /// know it, cannot be asked, or would be left with one admin whose
        /// key is on a token nobody else can replace. The old key is still
        /// moved aside, never overwritten
        #[arg(long)]
        force: bool,
    },
    /// Where this account's key actually is
    Status {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Publish the certificate chain proving the key is on hardware
    ///
    /// This is the half that is worth something to an auditor. Without it
    /// the mesh knows an agent signed; with it the mesh knows the key was
    /// generated inside a device of a named model and cannot be exported,
    /// and `meshnode users` grows a HARDWARE column.
    ///
    /// Export the chain from the device, leaf first:
    ///
    ///   ykman piv keys attest 9a --format DER leaf.der
    ///   ykman piv certificates export f9 --format DER intermediate.der
    ///
    /// The mesh must already trust the authority at the end of it —
    /// `meshnode users attest-root add` (admin).
    Attest {
        /// Certificate files, DER, leaf first then intermediates
        #[arg(value_name = "CERT", required = true)]
        certs: Vec<PathBuf>,
        /// piv | openpgp | fido2
        #[arg(long, default_value = "piv")]
        kind: String,
        /// Where on the device, as the device names it
        #[arg(long, default_value = "9a")]
        slot: String,
    },
    /// Withdraw an attestation: yours, or as an admin, somebody else's
    Unattest {
        /// Username or `mwu1…` prefix. Omit for your own.
        #[arg(long)]
        user: Option<String>,
        /// Why, recorded in the audit log
        #[arg(long, default_value = "")]
        reason: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum NodesCmd {
    /// List mesh nodes (default)
    Ls,
    /// Decide whether the mesh may place work on a node (admin)
    ///
    /// The owner's half of "is this machine part of my fleet". A node
    /// announces `no-execute` about *itself* and can take it back off on
    /// its next boot; this is the mesh's answer, it is signed by an admin,
    /// and the node cannot clear it. Use it when somebody joins from a
    /// machine you are happy to have a person on and not work on.
    ///
    /// A node nobody has decided about may run work, so an existing mesh is
    /// unchanged until somebody says otherwise.
    ///
    /// Nomad calls this eligibility: nothing new is placed there, and what
    /// already runs there stays.
    #[command(visible_alias = "eligibility")]
    Contribute {
        /// Node id (mw1…), unique prefix, or name
        node: String,
        /// Allow the mesh to place work here
        #[arg(long, conflicts_with = "off")]
        on: bool,
        /// Stop the mesh placing work here. It stays a full member:
        /// state, submission and terminals are unaffected
        #[arg(long)]
        off: bool,
        /// Shown wherever the refusal is. A node that is not taking work
        /// with no reason attached is a support ticket
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Tag a node (admin) — this one, or any other node in the mesh
    #[command(
        after_help = "A node's tags have two authors, and only one of them is anybody\n\
                      else's to write.\n\n\
                      Without a node this edits **this** machine's own announcements — the\n\
                      only list a machine speaks for. The reserved `no-execute` tag here\n\
                      makes it a non-executing peer.\n\n\
                      With a node the tags are **assigned in the mesh**: signed by you,\n\
                      replicated, and kept through that machine's reboots and rebuilds,\n\
                      because `prod` is your division of your own estate rather than\n\
                      something the machine knows about itself. The two halves are unioned\n\
                      wherever a tag is read, so `--on tag=prod` sees both.\n\n\
                      Tags only the machine can know — `cap.*`, `draining`, `console`,\n\
                      `relay` — are refused, and the refusal names the verb that does the\n\
                      real thing.\n\n  \
                      meshnode nodes tag --add git-sync\n  \
                      meshnode nodes tag web-07 --add prod --add dc=east\n  \
                      meshnode nodes tag web-07 --rm prod\n  \
                      meshnode nodes tag web-07 --clear"
    )]
    Tag {
        /// Node id (mw1…), unique prefix, or name. Omit for this node
        node: Option<String>,
        /// Tags to add (repeatable)
        #[arg(long = "add", value_name = "TAG")]
        add: Vec<String>,
        /// Tags to remove (repeatable)
        #[arg(long = "rm", value_name = "TAG")]
        rm: Vec<String>,
        /// Remove every tag assigned to this node. Its own announcements
        /// are untouched — only this node can change those
        #[arg(long, conflicts_with_all = ["add", "rm"])]
        clear: bool,
    },
    /// Move a node's deployment instances off it, without stopping it (admin)
    #[command(
        after_help = "Nomad's drain. The node takes no new work, and each deployment's\n\
                      instances there move — a replacement starts elsewhere and the original\n\
                      stops once the replacement has proved itself, under that deployment's\n\
                      `migrate` rules — until the deadline, when whatever is left is stopped.\n\
                      Other jobs already running there finish. The node stays a member and\n\
                      its daemon keeps running; `--lift` makes it eligible again.\n\n\
                      This is the mesh's decision, signed by you and kept through the node's\n\
                      restarts. `meshnode drain`, run on a node, is that node shutting down.\n\n  \
                      meshnode nodes drain web-07\n  \
                      meshnode nodes drain web-07 --deadline 4h --keep-system\n  \
                      meshnode nodes drain web-07 --lift"
    )]
    Drain {
        /// Node id (mw1…), unique prefix, or name
        node: String,
        /// How long instances have to move before they are stopped (e.g. 30m)
        #[arg(long, value_name = "DURATION", default_value = "1h", conflicts_with_all = ["no_deadline", "lift"])]
        deadline: String,
        /// Wait for instances to move however long it takes
        #[arg(long, conflicts_with = "lift")]
        no_deadline: bool,
        /// Leave `count = "system"` instances running there
        #[arg(long, conflicts_with = "lift")]
        keep_system: bool,
        /// End the drain: the node takes work again
        #[arg(long)]
        lift: bool,
    },
    /// Show, or repair, the addresses the mesh records for a node
    ///
    /// Without `--set` this prints what is recorded. With `--set` the
    /// founder rewrites another node's record — the one repair the
    /// membership rules allow, and the way out of a mesh where every
    /// recorded address went stale at once (a re-addressed VLAN), so no
    /// node can reach any other and the usual self-healing has nowhere to
    /// start. The subject overwrites this with its own announcement as
    /// soon as the two can talk.
    Addr {
        /// Node id (mw1…), unique prefix, or name
        node: String,
        /// Address this node answers on, `host:port` (repeatable).
        /// Replaces what the mesh records. A hostname is re-resolved on
        /// every dial, so it survives the next renumbering
        #[arg(long = "set", value_name = "HOST:PORT")]
        set: Vec<String>,
    },
    /// Isolate a suspected node, reversibly (admin)
    #[command(
        after_help = "The security verb, and the one to reach for when you do not yet know.\n\n\
                      A quarantined node takes no new work **and gives nobody a terminal**.\n\
                      That second half is the difference from `nodes contribute off`, which\n\
                      is a capacity decision and still lets an on-call engineer shell in.\n\n\
                      Unlike `nodes revoke` it is reversible, which is the point: most\n\
                      suspicions turn out to be nothing, and revoking a node's identity to\n\
                      find that out costs a re-enrolment.\n\n\
                      Examples:\n  \
                      meshnode nodes quarantine web-07 --reason \"unexpected outbound to 5.6.7.8\"\n  \
                      meshnode nodes quarantine web-07 --lift\n\n\
                      Work already running is not killed — `meshnode drain` is the verb for\n\
                      that, and you may want the process list first."
    )]
    Quarantine {
        /// Node id (mw1…), unique prefix, or name
        node: String,
        /// Why. Required when isolating
        #[arg(long, default_value = "")]
        reason: String,
        /// Lift an existing quarantine
        #[arg(long, conflicts_with = "reason")]
        lift: bool,
    },
    /// Permanently expel a node from the mesh (admin; irreversible)
    Revoke {
        /// Node id (mw1…), unique prefix, or name
        node: String,
        /// Why, recorded in the audit log
        #[arg(long, default_value = "revoked by admin")]
        reason: String,
        /// Required: revocation is terminal
        #[arg(long)]
        yes: bool,
    },
    /// What differs between two nodes — kernel, libc, drivers, packages,
    /// everything their collectors report — for "it only fails on that
    /// one box"
    ///
    /// Reads the replicated facts, so it works from any node about any
    /// two. Rows whose key looks like a version or a driver come first.
    Diff {
        /// A node name or id prefix
        left: String,
        /// The other one
        right: String,
        /// One collector only (default: every collector both report)
        #[arg(long)]
        collector: Option<String>,
        /// Print the keys that agree as well
        #[arg(long)]
        all: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum ResultsCmd {
    /// What changed between two runs of the same thing: which hosts answer
    /// differently, which started failing, which started passing
    ///
    /// Two fan-out groups (`run --fanout`) compare by node; two parallel
    /// or map runs compare by item. Output is compared by a digest of
    /// stdout taken when the run happened — for a fan-out that needs the
    /// run to have been launched with --json or --outputs, and for a map
    /// the object ids stand in.
    Diff {
        /// The earlier run: a fan-out group or a parallel job id
        before: String,
        /// The later run
        after: String,
        /// List the rows that did not change as well
        #[arg(long)]
        all: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum UsersCmd {
    /// Bind a user's public key to a username with an initial role grant
    Add {
        /// The person's name in this mesh
        username: String,
        /// The user's public key (mwu1…) — they can print it with `meshnode whoami`
        #[arg(long)]
        key: String,
        /// admin | operator | developer | auditor | viewer
        #[arg(long)]
        role: String,
        /// Node selector the grant is scoped to (any | name=… | tag=… | id=…)
        #[arg(long, default_value = "any")]
        nodes: String,
        /// Service-name glob the grant is scoped to
        #[arg(long, default_value = "*")]
        services: String,
        /// Grant lifetime (e.g. 30d, 12h); default: no expiry
        #[arg(long, value_name = "DURATION")]
        expires: Option<String>,
    },
    /// List users (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Replace a person's key, carrying their roles across (admin)
    #[command(
        after_help = "For a lost laptop, a suspected compromise, or a key-rotation policy.\n\n\
                      Binds the new key to the same username, moves every active grant onto\n\
                      it, then revokes the old one — **in that order**, so the person can act\n\
                      at every moment in between. The reverse order locks somebody out of\n\
                      their own mesh.\n\n\
                      The person generates the new key on their machine and reads it out:\n  \
                      meshnode whoami            # on their machine, prints mwu1…\n  \
                      meshnode users rotate mia --new-key mwu1…\n\n\
                      Their audit history stays under their old key and stays readable —\n\
                      every row carries the username as it was at the time. Rotation is not\n\
                      a way to disown what a key did."
    )]
    Rotate {
        /// The username whose key is being replaced
        username: String,
        /// Their new public key (mwu1…), from `meshnode whoami`
        #[arg(long)]
        new_key: String,
        /// Why, for the audit log
        #[arg(long, default_value = "key rotation")]
        reason: String,
    },
    /// Permanently revoke a user key (irreversible)
    Revoke {
        /// Username or mwu1… id
        user: String,
        /// Why, recorded in the audit log
        #[arg(long, default_value = "revoked by admin")]
        reason: String,
        /// Required: revocation is terminal
        #[arg(long)]
        yes: bool,
    },
    /// People waiting to be recognised by this mesh
    Pending {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Approve a pending request: bind the key to a username and grant a role
    Approve {
        /// The code from `meshnode users pending` (BRAVE-OTTER-4417), the
        /// requester's mwu1… key, or the username they suggested
        request: String,
        /// Username to bind, if not the one they suggested
        #[arg(long)]
        username: Option<String>,
        /// admin | operator | developer | auditor | viewer
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Node selector the grant is scoped to (any | name=… | tag=… | id=…)
        #[arg(long, default_value = "any")]
        nodes: String,
        /// Service-name glob the grant is scoped to
        #[arg(long, default_value = "*")]
        services: String,
        /// Grant lifetime (e.g. 30d, 12h); default: no expiry
        #[arg(long, value_name = "DURATION")]
        expires: Option<String>,
    },
    /// Decline a pending request
    Decline {
        /// The code, the mwu1… key, or the suggested username
        request: String,
        /// Why, recorded in the audit log
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Certificate authorities this mesh believes about hardware
    ///
    /// A hardware attestation is only worth the authority behind it, so the
    /// mesh keeps its own list rather than trusting whatever certificates
    /// happened to be compiled into somebody's binary. Trusting one is an
    /// admin act that lands in the op log, exactly like trusting a release
    /// signing key — and a mesh that trusts none verifies no attestations
    /// at all, which is the honest empty state.
    AttestRoot {
        #[command(subcommand)]
        cmd: AttestRootCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum AttestRootCmd {
    /// Authorities this mesh trusts (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Trust one (admin)
    ///
    /// Takes X.509 DER. Yubico publishes its PIV attestation root; an
    /// organisation minting its own cards adds its own issuing CA here and
    /// needs no new build to do it.
    Add {
        /// The certificate, DER
        #[arg(value_name = "CERT")]
        cert: PathBuf,
        /// What to call it in a list
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Stop trusting one (admin)
    ///
    /// The attestations it vouched for stop verifying with it, which is the
    /// point. They are not deleted — an auditor asks *when* trust ended.
    Rm {
        /// The id, as `attest-root ls` prints it
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum OidcCmd {
    /// Configure or replace a provider (admin)
    #[command(name = "providers")]
    Providers {
        #[command(subcommand)]
        cmd: Option<OidcProvidersCmd>,
    },
}

#[derive(Subcommand)]
pub(crate) enum OidcProvidersCmd {
    /// List configured providers (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Configure or replace one (admin)
    Put {
        /// Short name operators will type: `entra`, `okta`
        name: String,
        /// The `iss` claim tokens must carry, exactly
        #[arg(long)]
        issuer: String,
        /// This mesh's registered client id (the `aud` claim)
        #[arg(long)]
        client_id: String,
        /// Where the signing keys live (https)
        #[arg(long)]
        jwks_uri: String,
        /// RFC 8628 device-authorization endpoint (https)
        #[arg(long)]
        device_endpoint: String,
        /// Token endpoint (https)
        #[arg(long)]
        token_endpoint: String,
        /// Extra scopes beyond `openid` (repeatable)
        #[arg(long = "scope", value_name = "SCOPE")]
        scopes: Vec<String>,
        /// Which claim carries group membership
        #[arg(long, default_value = "groups")]
        groups_claim: String,
        /// Suggested role per group: `platform=operator` (repeatable).
        /// A suggestion shown to the approving admin, never an authorization
        #[arg(long = "group-role", value_name = "GROUP=ROLE")]
        group_roles: Vec<String>,
    },
    /// Stop believing a provider (admin)
    Rm { name: String },
}

#[derive(Subcommand)]
pub(crate) enum ServiceAccountsCmd {
    /// Mint a machine credential: keypair, user, marker and grant, in one go
    Create {
        /// A name for the system that will hold it, e.g. `ci-deploy`
        name: String,
        /// operator | developer | auditor | viewer (never admin)
        #[arg(long, default_value = "developer")]
        role: String,
        /// Node selector the grant is scoped to (any | name=… | tag=… | id=…)
        #[arg(long = "on", default_value = "any", value_name = "SELECTOR")]
        nodes: String,
        /// Service-name glob the grant is scoped to
        #[arg(long, default_value = "*")]
        services: String,
        /// How long this credential may act — e.g. 90d, 12h. Required
        /// unless you say --never-expires and mean it
        #[arg(long, value_name = "DURATION", conflicts_with = "never_expires")]
        expires_in: Option<String>,
        /// A credential with no clock on it. This is the long-lived bearer
        /// token the feature exists to replace, so it has to be typed
        #[arg(long, conflicts_with = "expires_in")]
        never_expires: bool,
        /// What this is for, and who to ask about it
        #[arg(long, default_value = "")]
        description: String,
        /// Where to write the private key (default: ./<name>.key)
        #[arg(long, value_name = "PATH")]
        out: Option<String>,
    },
    /// List machine principals and the clock each is on (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum GrantsCmd {
    /// Grant a role to an existing user
    Add {
        /// Username or mwu1… id
        user: String,
        /// The role to grant: admin, operator, developer, auditor or viewer
        #[arg(long)]
        role: String,
        /// Where the grant applies, as a selector: any, tag=…, name=…
        #[arg(long, default_value = "any")]
        nodes: String,
        /// Which services it covers: * for all, or a name
        #[arg(long, default_value = "*")]
        services: String,
        /// When the grant lapses, e.g. 30d (default: never)
        #[arg(long, value_name = "DURATION")]
        expires: Option<String>,
    },
    /// List grants (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Revoke one grant by id
    Revoke {
        /// grant-… id from `meshnode grants ls`
        grant: String,
        /// Confirm. Required, because this takes effect at once, everywhere
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum SecretsCmd {
    /// Seal a secret for the mesh (value never leaves this machine unencrypted)
    ///
    /// Without a scope, a secret is sealed for every member except
    /// consoles, no-execute and quarantined nodes, and anyone who may run a
    /// job on one of those can read it. `--on`, `--user` and `--role` set
    /// its scope first (see `secret scope`).
    Set {
        /// The secret's name
        name: String,
        /// Literal value (prefer --from-file or stdin to keep it out of shell history)
        #[arg(long)]
        value: Option<String>,
        /// Read the value from a file
        #[arg(long, value_name = "FILE")]
        from_file: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Set where a secret goes and who may use it (admin)
    ///
    /// Narrowing a secret's nodes does not take it back from nodes that
    /// already hold this value — set the value again for that. Widening is
    /// picked up by the mesh on its own.
    Scope {
        /// The secret's name
        name: String,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Remove the scope: back to the default nodes, and any user
        #[arg(long)]
        clear: bool,
    },
    /// List secret names and metadata (never values)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Remove a secret
    Rm {
        /// The secret's name
        name: String,
        /// Confirm. Required, because this takes effect at once, everywhere
        #[arg(long)]
        yes: bool,
    },
    /// List configured external secret providers on the local node
    Providers {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum InviteCmd {
    /// Mint an invite token for new nodes to join with
    Create {
        /// How long the invite can be used, e.g. 20m, 24h
        #[arg(long, default_value = "24h", value_name = "DURATION")]
        ttl: String,
        /// How many machines may join with it
        #[arg(long, default_value = "1")]
        uses: u32,
        /// A tag every machine that joins with it gets (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Extra bootstrap address to put first in the token (repeatable) —
        /// e.g. a public address the joiner can reach that this node does
        /// not know about
        #[arg(long = "addr", value_name = "HOST:PORT")]
        addrs: Vec<String>,
    },
    /// Approve a nearby node waiting in `meshnode join --pair`: discover it
    /// over mDNS, compare 6-digit codes, and admit it (admin)
    Approve {
        /// Candidate node name (or id prefix) when several are waiting
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// Pair by address, skipping mDNS discovery (multicast-hostile LANs)
        #[arg(long, value_name = "HOST:PORT")]
        addr: Option<String>,
        /// How long to browse the LAN for candidates
        #[arg(long, value_name = "DURATION", default_value = "3s")]
        browse: String,
        /// Tags for the new node (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// How to draw the confirmation code (see `meshnode join --help`)
        #[arg(long, value_enum, default_value = "auto")]
        qr: QrStyle,
    },
    /// List invites and how many uses each has left (admin)
    Ls {
        /// Include invites that have expired, been used up or been revoked
        #[arg(long)]
        all: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Revoke an invite, so its token stops working on every node (admin)
    ///
    /// For a token that was pasted somewhere it should not have been. Nodes
    /// already admitted with it stay; use `meshnode revoke` for those.
    Revoke {
        /// Invite id (inv-…) or a unique prefix of one, as `invite ls`
        /// prints it
        invite: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum JobsCmd {
    /// List jobs (default)
    ///
    /// Paged and filtered at the daemon: the whole table is never shipped,
    /// so this stays fast — and stays *possible* — on a mesh with a long
    /// memory.
    Ls {
        /// Only jobs in this state: queued, running, succeeded, failed,
        /// cancelled, timed-out — or `live` / `terminal`
        #[arg(long, value_name = "STATE")]
        state: Option<String>,
        /// Only jobs whose command contains this text
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,
        /// How many to show, newest first
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Skip this many, for paging
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Show one job
    Show { job: String },
    /// Stream a job's logs
    Logs {
        /// The job's id, or enough of the start of it to be unique
        job: String,
        /// Keep printing output until the job ends
        #[arg(long)]
        follow: bool,
    },
    /// Block until a job ends, then exit with its code — and, if asked,
    /// run something or send a notification
    ///
    /// Polls the replicated summary, so it works from any node for a job
    /// running anywhere, and survives the node you launched from going
    /// away.
    #[command(after_help = "Examples:\n  \
                      meshnode jobs wait job-9x2 && ./publish.sh\n  \
                      meshnode jobs wait job-9x2 --then 'say \"training finished\"'\n  \
                      meshnode jobs wait job-9x2 --notify")]
    Wait {
        /// The job's id, or enough of the start of it to be unique
        job: String,
        /// Run this with `sh -c` once the job ends, with MESHWARE_JOB,
        /// MESHWARE_STATE, MESHWARE_EXIT and MESHWARE_NODE in its environment
        #[arg(long, value_name = "COMMAND")]
        then: Option<String>,
        /// Give up waiting after this long (e.g. 2h, 30m)
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,
        /// Send the finish through `[notify]` in meshnode.toml
        #[arg(long)]
        notify: bool,
    },
    /// Cancel a running job
    Cancel {
        /// The job's id, or enough of the start of it to be unique
        job: String,
        /// Record the job as cancelled even if its executor cannot be
        /// reached (crashed, wiped, gone). A reachable executor is always
        /// asked first; if it later reports what really happened, that
        /// report wins. Rewriting the record needs operator or admin;
        /// developers can cancel their own jobs without it.
        #[arg(long)]
        force: bool,
    },
    /// Why a job landed where it did, term by term — and what the cache had
    /// to say about it
    Explain {
        /// The job's id, or enough of the start of it to be unique
        job: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Forget every job that has already finished, mesh-wide
    Clear {
        /// Required: this deletes the record everywhere, for everyone
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CacheCmd {
    /// Hit rate, bytes saved and entries, per service (default)
    Stats {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Forget entries: everything, one key, or one service's
    Purge {
        /// An action key (act-…) or a service name. Omit for everything
        #[arg(value_name = "KEY|SERVICE")]
        target: Option<String>,
        /// Required: a purge costs re-execution across the whole mesh
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ScriptCmd {
    /// Print the contract this script would register with, and register nothing
    Inspect {
        /// The script to read
        path: String,
        /// Print the JSON Schema alone, for piping
        #[arg(long)]
        json: bool,
    },
    /// Write the script's `.definitions` file from what it already declares
    ///
    /// The upgrade path off `#meshware:` headers, and the terminal's twin of
    /// Director's *Write the file* button. Everything the script says about
    /// itself — including what could only be inferred from reading it —
    /// comes out as JSON beside it, ready to correct and extend with the
    /// things a comment has no room for.
    Definitions {
        /// The script to read
        path: String,
        /// Print to stdout instead of writing the file
        #[arg(long)]
        stdout: bool,
        /// Overwrite an existing definitions file
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ObjectsCmd {
    /// List the ephemeral objects this node holds (default)
    Ls,
    /// Fetch an object's bytes from wherever in the mesh it lives
    Get {
        /// Object id (obj-…) or unique prefix
        object: String,
        /// Write here instead of stdout
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum BundlesCmd {
    /// List bundles known to the mesh (default)
    Ls,
    /// List the files in a bundle (manifest fetched from a holder if needed)
    Show {
        /// Bundle id (bndl-…) or unique prefix
        bundle: String,
    },
    /// Print one file from a bundle to stdout (fetches the bundle if needed)
    Cat {
        /// Bundle id (bndl-…) or unique prefix
        bundle: String,
        /// Path inside the bundle, e.g. scripts/deploy.sh
        path: String,
    },
    /// Ask nodes to fetch a bundle now, before somebody is waiting on it
    Prefetch {
        /// Bundle id (bndl-…), a unique prefix, or a service name
        bundle: String,
        /// Which nodes (any | tag=… | name=… | id=… | fact.…)
        #[arg(long, default_value = "any")]
        on: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum ContentCmd {
    /// What this node holds and what is reclaimable (default)
    Status,
    /// Reclaim bundles nothing still needs
    Prune {
        /// Measure and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Ignore the retention window: take everything unreferenced now.
        /// Still cannot touch a bundle something needs.
        #[arg(long)]
        aggressive: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ServiceCmd {
    /// Register a service version bound to a bundle
    Create {
        /// Service name
        name: String,
        /// Runtime: bash | python | ansible | exec
        #[arg(long)]
        runtime: String,
        /// Bundle id (bndl-…) to bind
        #[arg(long)]
        bundle: String,
        /// Entry point relative to --working-dir (playbook / script /
        /// binary) — or, for `--runtime container`, the **image**:
        /// `docker.io/library/nginx:1.25`
        #[arg(long)]
        entrypoint: String,
        /// Working directory within the bundle — or, for a container, a
        /// path inside the image
        #[arg(long, default_value = "")]
        working_dir: String,
        /// Container only: override the image's own ENTRYPOINT, as
        /// Nomad's `config { command = … }` does. Without it the image
        /// runs whatever it was built to run
        #[arg(long, value_name = "PROGRAM")]
        command: Option<String>,
        /// Container only: one argument for --command. Repeatable, and
        /// order is kept: `--arg -c --arg 'echo hello, world'`
        ///
        /// `allow_hyphen_values` because an argument that starts with a
        /// dash is the *common* case here — `-c`, `--port`, `-v` — and
        /// without it clap eats the first one as a flag of its own.
        #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Path to a JSON Schema file describing inputs
        #[arg(long, value_name = "FILE")]
        contract: Option<String>,
        /// Static env KEY=VALUE (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// A short description, shown in listings
        #[arg(long, default_value = "")]
        description: String,
        /// A tag for the service (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// List services (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Show one service
    Show {
        /// The service's name
        name: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Run a service, streaming output live
    Run {
        /// The service's name
        name: String,
        /// Target selector
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// key=value inputs (validated against the contract)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Give up on the run after this many seconds
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
        /// Resolve and print what this would do, dispatching nothing
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
        /// e.g. 'contains(stdout, "ok")'
        #[arg(long, value_name = "EXPR")]
        expect: Option<String>,
        /// Serve this from the action cache if the same work has already
        /// been done anywhere in the mesh. A service whose version is
        /// tagged `cacheable` gets this without asking
        #[arg(long)]
        cacheable: bool,
        /// Files this service produces, comma-separated globs relative to
        /// its working directory. Part of the cache key, and what a hit
        /// materialises
        #[arg(long = "cache-outputs", value_name = "GLOB,...")]
        cache_outputs: Option<String>,
        /// Print the action key and every field that produced it, and
        /// dispatch nothing
        #[arg(long = "explain-key")]
        explain_key: bool,
        /// Accelerators this service needs.
        ///
        /// Rides `inputs` for the same reason `--cacheable` does:
        /// `RunService` is re-decoded from a signed envelope on the
        /// executor and may never grow a field. The origin strips it
        /// before the contract is validated, so a schema with
        /// `additionalProperties: false` never sees it.
        #[arg(long, value_name = "N")]
        gpus: Option<u32>,
        /// Minimum memory on each device, in MB
        #[arg(long = "vram-mb", value_name = "MB")]
        vram_mb: Option<u64>,
        /// Device selector: `kind=gpu,model=~4090,vendor=nvidia`. `~x`
        /// means "contains x"
        #[arg(long, value_name = "K=V,...")]
        device: Option<String>,
        /// Tolerate co-tenants on the devices this service is given
        #[arg(long = "gpu-share")]
        gpu_share: bool,
        /// Run as an all-or-nothing gang of N nodes: every member starts at
        /// the same moment, or none of them does
        #[arg(long, value_name = "N")]
        gang: Option<u32>,
        /// Gang layout: `pack` onto the fewest nodes (default) or `spread`
        /// one per node for fault tolerance
        #[arg(long = "gang-placement", value_name = "pack|spread")]
        gang_placement: Option<String>,
        /// Re-run the whole gang if a member dies, instead of failing it
        #[arg(long = "gang-restart")]
        gang_restart: bool,
    },
    /// Roll the service's active version back (or forward) to VERSION
    Rollback { name: String, version: u32 },
    /// Retire a service
    Remove { name: String },
}

/// `meshnode sessions …`
#[derive(Subcommand)]
pub(crate) enum SessionCmd {
    /// List live sessions (the default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Close a live session by id
    Close { id: u64 },
    /// Who may open a terminal, and where
    #[command(subcommand)]
    Policy(SessionPolicyCmd),
}

/// `meshnode sessions policy …`
#[derive(Subcommand)]
pub(crate) enum SessionPolicyCmd {
    /// List session policies
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Publish (or replace) a policy
    #[command(
        after_help = "A terminal needs *both* a runner grant on the node and a policy naming\n\
                      the user and the node. Nobody gains a prompt by a policy alone.\n\n\
                      Examples:\n  \
                      meshnode sessions policy put dev --users '*' --nodes tag=dev --allow shell\n  \
                      meshnode sessions policy put oncall --users 'ops-*' --nodes any --allow shell"
    )]
    Put {
        /// Policy name
        id: String,
        /// Username glob: `*`, `ada`, `ops-*`
        #[arg(long, default_value = "*")]
        users: String,
        /// Node selector: `any`, `tag=dev`, `name=hermes`
        #[arg(long, default_value = "any")]
        nodes: String,
        /// What it permits: shell | shell-privileged (repeatable)
        #[arg(long = "allow", value_name = "PERMISSION", required = true)]
        allow: Vec<String>,
        /// A short description, shown in listings
        #[arg(long, default_value = "")]
        description: String,
        /// Stop applying after this long, e.g. 8h, 30d
        #[arg(long, value_name = "DURATION")]
        expires_in: Option<String>,
    },
    /// Retire a policy
    Rm { id: String },
}

/// `meshnode facts …` — what the fleet says about itself (M18 §4.9).
///
/// One variant is much larger than its siblings, which clippy notices. Boxing it
/// would mean boxing a value clap constructs once per process to save a few
/// hundred bytes of stack in a CLI dispatch — the same trade every other
/// subcommand enum here declines.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub(crate) enum FactsCmd {
    /// What is known about one subject (default: this node)
    Show {
        /// A node name or node id prefix
        subject: Option<String>,
        /// One collector only
        #[arg(long)]
        collector: Option<String>,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Every subject whose facts satisfy a comparison
    ///
    /// `os_id=debian`, `kernel_release<6.1`, `gpu_model~=A100`. The
    /// operators are `= != < <= > >= ~=`, where `~=` is substring.
    Query {
        /// The comparison, with or without the `fact.` prefix
        expression: String,
        /// Only this collector's facts
        #[arg(long)]
        collector: Option<String>,
        /// Print only how many matched
        #[arg(long)]
        count: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// What changed about a subject, and when
    Diff {
        /// The node or host whose facts to compare over time
        subject: String,
        /// Only this collector's facts
        #[arg(long)]
        collector: Option<String>,
        /// Only changes newer than this (e.g. 24h, 7d)
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// How many to show
        #[arg(long, default_value = "50")]
        limit: u32,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum CollectorsCmd {
    /// List collectors (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Publish a collector from a definition file (YAML or JSON)
    Add {
        /// Path to the definition, or `-` for stdin
        file: String,
    },
    /// Stop a collector running, keeping it and its facts
    Pause { name: String },
    /// Arm a paused collector again
    Resume { name: String },
    /// How a collector's ring is doing — where it runs, for how long, and
    /// what to widen it to next
    Status { name: String },
    /// Widen (or narrow) which nodes run a collector — the deployment step
    Widen {
        /// The collector's name
        name: String,
        /// The new selector, e.g. `tag=gpu` after a canary on
        /// `tag=gpu, tag=canary`
        #[arg(long = "to", value_name = "SELECTOR")]
        to: String,
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum GangCmd {
    /// Gangs holding or waiting for reservations, most senior first
    Ls {
        /// Include gangs that have finished
        #[arg(long)]
        all: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// One gang: where its members landed, and who is ahead of it
    Show {
        /// The gang's id
        #[arg(value_name = "GANG-ID")]
        gang: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
}

#[derive(Subcommand)]
pub(crate) enum DnsCmd {
    /// Route .mesh to this machine's responder, and take back the low-port
    /// grant the retired mesh proxy was given. Needs root
    Install {
        /// The responder's address
        #[arg(long, default_value = "127.0.0.1:8600")]
        address: String,
        /// Print what would be written and run
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum DeploymentCmd {
    /// Publish a deployment from a definition file (TOML, YAML or JSON)
    Put {
        /// Path to the definition, or `-` for stdin
        #[arg(value_name = "FILE")]
        file: String,
        /// Print what it would do — matching nodes, health, problems —
        /// without publishing anything
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        watch: RolloutWatch,
    },
    /// List deployments (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Where every deployment's instances run, machine by machine, and how
    /// much of each machine they asked for
    Placement {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Show one deployment, with its definition
    Show {
        /// The deployment's name
        name: String,
        /// table | json | toml (toml round-trips into `deployments put`)
        #[arg(long, default_value = "table")]
        output: String,
    },
    /// Desired vs running vs healthy, per instance, live
    Status {
        /// The deployment's name
        name: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
        /// Also measure what each instance is using right now, on its node
        #[arg(long)]
        stats: bool,
    },
    /// Roll the deployment forward to a new service version
    Rollout {
        /// The deployment's name
        name: String,
        /// The new pinned service version, e.g. `collector@8`
        #[arg(value_name = "SERVICE@VERSION")]
        service: String,
        #[command(flatten)]
        watch: RolloutWatch,
    },
    /// Point the deployment back at a previous service version
    Rollback {
        /// The deployment's name
        name: String,
        /// The service version to roll back to; defaults to the one before
        /// the current
        #[arg(long, conflicts_with_all = ["generation", "tag"])]
        to: Option<u32>,
        /// Go back to everything published at this generation — the service
        /// version, ports, resources and rollout settings together. `deployment
        /// history` lists the ones kept
        #[arg(long, conflicts_with = "tag")]
        generation: Option<u32>,
        /// Go back to the version this tag names
        #[arg(long)]
        tag: Option<String>,
        #[command(flatten)]
        watch: RolloutWatch,
    },
    /// Stop reconciling a deployment, keeping it and its instances
    Pause { name: String },
    /// Resume a paused deployment
    Resume { name: String },
    /// Delete a deployment; the reconciler retires its instances
    Rm { name: String },
    /// Check a definition file without a mesh: parse it and say what it
    /// would do. Exits non-zero on the first problem, so it fits in CI
    Validate {
        /// Path to the definition, or `-` for stdin
        #[arg(value_name = "FILE")]
        file: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Print a complete, commented starting definition
    Init {
        /// The deployment's name
        #[arg(long, default_value = "web")]
        name: String,
        /// The image to run
        #[arg(long, default_value = "docker.io/library/nginx:alpine")]
        image: String,
    },
    /// Print the JSON Schema for definition files, for editors
    Schema,
    /// Change how many instances run, restarting none of the ones that stay
    Scale {
        /// The deployment's name
        name: String,
        /// The new number of instances
        #[arg(value_name = "COUNT")]
        count: u32,
        #[command(flatten)]
        watch: RolloutWatch,
    },
    /// The versions a deployment keeps — its last ten, and any older one a tag
    /// names — newest first, with what changed in each
    History {
        /// The deployment's name
        name: String,
        /// Show each change as `field: from → to` rather than just its name
        #[arg(short = 'p', long = "diff")]
        diff: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Name one version, which keeps it past the last ten and lets
    /// `rollback --tag` go back to it by name
    Tag {
        /// The deployment's name
        name: String,
        /// The name to give that version
        tag: String,
        /// The generation to name; defaults to the one running now
        #[arg(long)]
        generation: Option<u32>,
        /// What is special about this version
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Remove a tag. The version it named stays only while it is among the
    /// last ten
    Untag { name: String, tag: String },
    /// Let a held canary set cut over — for a deployment whose file set
    /// `update.auto_promote = false`
    Promote {
        /// The deployment's name
        name: String,
        #[command(flatten)]
        watch: RolloutWatch,
    },
    /// One instance up close: every incarnation it has had, its health, and
    /// what it is using right now
    Instance {
        /// The deployment's name
        name: String,
        /// A replica number, or the node name of a `system` deployment's
        /// instance
        slot: String,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Open a terminal inside a running container instance — the same grant,
    /// reason and audit as `meshnode shell`
    Exec {
        /// The deployment's name
        name: String,
        /// Which instance — a replica number, or the node name of a
        /// `system` deployment's instance. Needed when there is more than one
        #[arg(long, value_name = "SLOT")]
        slot: Option<String>,
        /// Why — a ticket, an incident, a sentence. Recorded in the audit log
        /// and covered by your signature
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Run this instead of `sh`
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Stream one instance's output: the only one, or the one in --slot
    Logs {
        /// The deployment's name
        name: String,
        /// Which instance — a replica number, or the node name of a
        /// `system` deployment's instance. Needed when there is more than one
        #[arg(long, value_name = "SLOT")]
        slot: Option<String>,
        /// Keep streaming as the instance writes
        #[arg(short, long)]
        follow: bool,
    },
    /// Replace instances without changing the definition: all of them, in
    /// batches under the rollout policy, or one with --slot
    Restart {
        /// The deployment's name
        name: String,
        /// Replace only this instance — a replica number, or the node name
        /// of a `system` deployment's instance
        #[arg(long, value_name = "SLOT")]
        slot: Option<String>,
        #[command(flatten)]
        watch: RolloutWatch,
    },
}

#[derive(Subcommand)]
pub(crate) enum SourcesCmd {
    /// Register a Git repository as a bundle source
    AddGit {
        /// Source name (unique)
        name: String,
        /// Clone URL (https, ssh, or a local path)
        #[arg(long)]
        url: String,
        /// Branch to track
        #[arg(long, default_value = "main")]
        branch: String,
        /// Poll interval (e.g. 60s, 5m)
        #[arg(long, value_name = "DURATION", default_value = "5m")]
        interval: String,
        /// Service to auto-bump onto each new bundle (repeatable)
        #[arg(long = "bump", value_name = "SERVICE")]
        bump: Vec<String>,
    },
    /// List sources (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Remove a source (bundles it produced stay)
    Rm {
        /// The source's name
        name: String,
        /// Confirm. Required, because this takes effect at once, everywhere
        #[arg(long)]
        yes: bool,
    },
    /// Sync a source right now on the local node
    Sync { name: String },
}

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    Check,
    Show,
    /// Every path this node reads or writes, and whether it exists
    Paths {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
}

/// `meshnode history …` — the indexed view of what this node has seen.
///
/// Separate from `meshnode jobs` on purpose. That answers "what is
/// happening", reads live replicated state, and is what you want at a
/// terminal right now. This answers "what happened", reads the local index,
/// and stays fast when the answer covers a year.
#[derive(Subcommand)]
pub(crate) enum HistoryCmd {
    /// Search jobs, newest first (default)
    Jobs {
        /// Only this outcome: succeeded, failed, cancelled, timed-out,
        /// running, queued
        #[arg(long)]
        state: Option<String>,
        /// Only jobs that ran on this node (id prefix or name)
        #[arg(long)]
        node: Option<String>,
        /// Only what happened since then, e.g. 90d, 12h, 2026-01-31
        #[arg(long, value_name = "WHEN")]
        since: Option<String>,
        /// Only what happened before then
        #[arg(long, value_name = "WHEN")]
        until: Option<String>,
        /// Substring of the command or the runtime
        #[arg(long, value_name = "TEXT")]
        grep: Option<String>,
        /// How many to show
        #[arg(long, default_value = "20")]
        limit: u32,
        /// Skip this many, to page through a long list
        #[arg(long, default_value = "0")]
        offset: u32,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// How much history this node is holding, and how far back it goes
    Stats {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// The rows one fan-out answered with
    ///
    /// Rows exist only for a run launched with `--json`, `--outputs` or
    /// `--expect`: nothing is captured unless somebody asked. Answered by
    /// the node that owns or owned the group — asked anywhere else it says
    /// so and names the owner, exactly as the queries above do.
    Results {
        /// The fan-out group id, as `meshnode run --fanout` printed it
        group: String,
        /// Only targets whose job did not succeed
        #[arg(long)]
        failed: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// What changed between two runs of the same thing: which hosts answer
    /// differently, which started failing, which started passing
    ///
    /// Two fan-out groups (`run --fanout`) compare by node; two parallel or
    /// map runs compare by item. Output is compared by a digest of stdout
    /// taken when the run happened.
    Diff {
        /// The earlier run: a fan-out group or a parallel job id
        before: String,
        /// The later run
        after: String,
        /// List the rows that did not change as well
        #[arg(long)]
        all: bool,
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value_t = Output::Table)]
        output: Output,
    },
    /// Read a job's output back from wherever a sink put it
    ///
    /// Opens at the **end**, because the reason anyone opens an old log is
    /// the thing that went wrong. The mesh fetches it — this machine does
    /// not need a route to the destination, and the node that exported it
    /// does. Opening writes one `history.replay` audit entry; paging back
    /// writes none.
    Replay {
        /// The job, by id or unique prefix
        job: String,
        /// Lines to skip back from the end
        #[arg(long, default_value = "0", value_name = "N")]
        from_end: u64,
        /// Lines to fetch
        #[arg(long, default_value = "500", value_name = "N")]
        limit: u64,
        /// Only lines containing this — delegated to the destination's own
        /// index where it has one
        #[arg(long, value_name = "TEXT")]
        grep: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ShedCmd {
    /// What is shed right now, and how much of the mesh will honour it
    Status {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Refuse work people submit, mesh-wide. Running jobs are unaffected
    Submissions {
        /// Take it off again
        #[arg(long)]
        off: bool,
        /// Why — shown in every refusal this causes
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Stop deployment reconciliation and schedule firing, mesh-wide
    Reconcile {
        /// Take it off again
        #[arg(long)]
        off: bool,
        /// Why — shown wherever this is reported
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Lift everything: take work again, reconcile again
    Clear,
}

#[derive(Subcommand)]
pub(crate) enum VolumeCmd {
    /// Ask this node's volume plugin to provision a volume
    Create {
        /// The name jobs will claim it by
        name: String,
        /// How much to ask the plugin for. Absent = the plugin decides
        #[arg(long, value_name = "MB")]
        capacity_mb: Option<u64>,
        /// Plugin-specific, as `key=value`. Repeatable. Validated before it
        /// leaves — shell metacharacters are refused here, so a plugin that
        /// forgets to quote is merely broken rather than exploitable
        #[arg(long = "param", value_name = "K=V")]
        params: Vec<String>,
        /// Lend it read-only whatever a job asks for
        #[arg(long)]
        read_only: bool,
    },
    /// Stop offering a volume this node provisioned, and ask the plugin to
    /// remove it
    Rm {
        /// The volume's name
        name: String,
        /// Forget it here even when the plugin refuses, for a volume whose
        /// backing store is already gone. Never overrides a live lease
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ScheduleCmd {
    /// Publish a schedule from a definition file (TOML, YAML or JSON)
    Put {
        /// Path to the definition, or `-` for stdin
        #[arg(value_name = "FILE")]
        file: String,
        /// Print what it would do — when it next fires, matching nodes,
        /// problems — without publishing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// List schedules (default)
    Ls {
        /// How to print it: `table`, or `json` for a script
        #[arg(short, long, value_enum, default_value = "table")]
        output: Output,
    },
    /// Show one schedule, with its definition
    Show {
        /// The schedule's name
        name: String,
        /// table | json | toml (toml round-trips into `schedule put`)
        #[arg(long, default_value = "table")]
        output: String,
    },
    /// Stop a schedule firing, keeping it and its history
    Pause { name: String },
    /// Resume a paused schedule
    Resume { name: String },
    /// Delete a schedule. Jobs it already fired are untouched.
    Rm { name: String },
}

/// How much of a pairing code to draw on this terminal.
#[derive(Clone, Copy, ValueEnum, Debug, PartialEq, Eq)]
pub(crate) enum QrStyle {
    /// A QR when stderr is a terminal, none when it is a log file.
    Auto,
    /// Unicode half-blocks: one character per module wide, square.
    Blocks,
    /// `##` per module, for a console with no block glyphs.
    Ascii,
    /// Digits and words only.
    Off,
}
