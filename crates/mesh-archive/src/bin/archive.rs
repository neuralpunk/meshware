//! `meshware-archive` — read a meshware Parquet archive without meshware.
//!
//! The archive is a directory of ordinary files, and this is the smallest
//! thing that proves it: a reader that never talks to a node, never opens a
//! socket, and works on a copy restored from a backup onto a machine that
//! has never been part of a mesh.
//!
//! It also archives. `ingest` connects to the local node over its IPC
//! socket, pages through the history index, and writes what has settled —
//! which is what makes a headless server able to keep long-term history
//! without Director and without putting an analytics engine in the daemon.
//! Run it from cron or a systemd timer; it is idempotent, so how often is
//! a matter of taste rather than correctness.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use mesh_archive::{Archive, Table};

#[derive(Parser)]
#[command(
    name = "meshware-archive",
    version,
    about = "Query a meshware Parquet archive",
    long_about = "Read-only SQL over an archived meshware history.\n\
                  Works on any copy of the directory, on any machine, with \
                  no node running."
)]
struct Cli {
    /// The archive directory (default: <node home>/archive — $MESHWARE_HOME,
    /// $MESHWARE_DATA_DIR, or ~/.meshware)
    #[arg(long, value_name = "DIR", env = "MESHWARE_ARCHIVE")]
    archive: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a SQL query against `jobs`, `audit`, `usage`, `facts` and `results`
    Query {
        /// The statement. Omit to list the example queries instead.
        sql: Option<String>,
        /// Emit JSON objects rather than a table
        #[arg(long)]
        json: bool,
    },
    /// What this archive holds
    Info,
    /// Merge fragmented date partitions into one file each
    Compact,
    /// Delete date partitions older than the mesh's `archive` window
    ///
    /// The window is a promise about a tier, and until this existed it was
    /// a number `retention show` printed that nothing applied. Whole days
    /// only: rows here are immutable and a day is the unit everything is
    /// written and read in.
    Prune {
        /// The node's home, for reading the mesh's `archive` window
        /// (default: $MESHWARE_HOME, else ~/.meshware)
        #[arg(long, value_name = "DIR")]
        home: Option<PathBuf>,
        /// Use this many days instead of asking the node
        #[arg(long, value_name = "DAYS")]
        days: Option<u32>,
        /// Say what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy settled history out of the local node and into Parquet.
    ///
    /// Idempotent: rows already archived are skipped, so running it hourly
    /// and running it once a week differ only in how fresh the archive is.
    Ingest {
        /// The node's home, for finding its IPC socket
        /// (default: $MESHWARE_HOME, else ~/.meshware)
        #[arg(long, value_name = "DIR")]
        home: Option<PathBuf>,
        /// Skip the compaction pass afterwards
        #[arg(long)]
        no_compact: bool,
        /// Also replicate the archive to a bucket, e.g. `s3://acme-archive/prod`
        ///
        /// Idempotent: part filenames are deterministic, so a part already
        /// there is skipped. Safe to run on any schedule from any machine.
        #[arg(long, value_name = "URL")]
        bucket: Option<String>,
        /// S3-compatible endpoint (MinIO, Ceph, R2). Omit for AWS.
        #[arg(long, value_name = "URL", requires = "bucket")]
        endpoint: Option<String>,
        #[arg(long, value_name = "NAME", requires = "bucket")]
        region: Option<String>,
        /// Bucket credentials as a `{{secret:name}}` reference, whose value
        /// is `ACCESS_KEY_ID:SECRET_ACCESS_KEY`
        ///
        /// Optional: without it the credentials come from the environment,
        /// which is what an instance profile, IRSA or `AWS_PROFILE`
        /// already fill in. Either way, **never from argv** — a key on a
        /// command line is readable from `/proc/<pid>/cmdline`.
        #[arg(long, value_name = "REF", requires = "bucket")]
        credential: Option<String>,
        /// Write the audit table under S3 Object Lock, **compliance** mode
        ///
        /// Compliance, never governance: governance mode is bypassable by
        /// anyone holding `s3:BypassGovernanceRetention`, which is usually
        /// the account that set the bucket up. Object Lock must have been
        /// enabled when the bucket was created — it cannot be turned on
        /// afterwards — and this refuses rather than silently writing an
        /// ordinary object.
        #[arg(long, requires = "bucket")]
        worm: bool,
        /// Days an audit object stays locked. Defaults to this mesh's own
        /// audit retention window, floored at a year.
        #[arg(long, value_name = "DAYS", requires = "worm")]
        worm_days: Option<u32>,
    },
    /// Check every archived audit row's signature
    ///
    /// The point of exporting the trail with its proofs: this reads the
    /// archive — including a copy pulled out of a bucket onto a machine
    /// that has never been in the mesh — and verifies each row against the
    /// public key carried in its own `node` column.
    ///
    /// Exits non-zero if anything fails, so it can be a cron job rather
    /// than a thing somebody remembers to look at.
    Verify {
        /// Print every row, not only the failures
        #[arg(long)]
        all: bool,
    },
    /// Replicate an already-built archive to a bucket, without ingesting
    ///
    /// The same push `ingest --bucket` does, for a machine that holds a
    /// copy of the directory and no node at all.
    Push {
        #[arg(long, value_name = "URL")]
        bucket: String,
        #[arg(long, value_name = "URL")]
        endpoint: Option<String>,
        #[arg(long, value_name = "NAME")]
        region: Option<String>,
        /// `{{secret:name}}` holding `ACCESS_KEY_ID:SECRET_ACCESS_KEY`
        #[arg(long, value_name = "REF")]
        credential: Option<String>,
        #[arg(long)]
        worm: bool,
        #[arg(long, value_name = "DAYS", requires = "worm")]
        worm_days: Option<u32>,
    },
}

/// `<home>/archive`, with the home found **by `mesh-core`'s own rule** —
/// the one `ingest` uses to find the node, and the one `meshnode doctor`
/// uses to look for the archive.
///
/// This used to be a copy of that rule that knew `MESHWARE_HOME` and not
/// `MESHWARE_DATA_DIR`, which is the variable the shipped unit sets. So the
/// packaged timer ingested into `/var/lib/meshware/.meshware/archive` (the
/// service account's `$HOME`, plus `.meshware`), while `doctor` looked in
/// `/var/lib/meshware/archive`, found nothing, and warned forever that no
/// archive had ever been written.
fn default_archive() -> PathBuf {
    mesh_core::layout::Layout::resolve(None).archive_dir()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = cli.archive.unwrap_or_else(default_archive);

    let runtime = match tokio_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(&root, cli.command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// The local mesh's id, if there happens to be a node's home beside us.
///
/// **Best-effort, and never an error.** The reader's whole promise is that
/// it works on a copy of the directory on a machine that has never been part
/// of a mesh, so "there is no genesis here" is an ordinary answer. It is
/// used for one thing on the read path: projecting `mesh_id` onto rows from
/// a format-1 archive that predates the column.
fn local_mesh_id(home: Option<&PathBuf>) -> Option<String> {
    use mesh_core::config::{NodeConfig, Overrides};
    let config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: home.cloned(),
            ..Default::default()
        },
    )
    .ok()?;
    let genesis = mesh_core::genesis::Genesis::load(&config.data_dir).ok()??;
    Some(genesis.body.mesh_id.to_display_string())
}

async fn run(root: &PathBuf, command: Command) -> Result<(), String> {
    let archive = Archive::open(root).map_err(|e| e.to_string())?;

    match command {
        Command::Info => {
            let manifest = archive.manifest().map_err(|e| e.to_string())?;
            let (bytes, files) = archive.disk_usage();
            println!("archive   {}", root.display());
            if !archive.exists() {
                println!("          (empty — nothing archived here yet)");
                return Ok(());
            }
            println!(
                "mesh      {}",
                manifest
                    .mesh_id
                    .as_deref()
                    .unwrap_or("(not recorded — written before format 2; `ingest` stamps it)")
            );
            println!("format    {}", manifest.format);
            println!("jobs      {}", manifest.jobs_written);
            println!("files     {files}");
            println!("size      {:.1} MB", bytes as f64 / (1024.0 * 1024.0));
            if archive.has_legacy_parts() {
                println!(
                    "\nSome part files here predate the `mesh_id` column. They read \
                     with it projected\nfrom this machine's mesh{}; compaction rewrites \
                     them into the current shape.",
                    match local_mesh_id(None) {
                        Some(id) => format!(" ({id})"),
                        None => ", and read null where there is no local mesh".into(),
                    }
                );
            }
            Ok(())
        }
        Command::Ingest {
            home,
            no_compact,
            bucket,
            endpoint,
            region,
            credential,
            worm,
            worm_days,
        } => {
            // Writing needs the identity, so unlike the read paths this one
            // refuses without it: a `mesh_id` guessed at write time is a
            // wrong answer baked into an immutable file.
            let mesh_id = local_mesh_id(home.as_ref()).ok_or_else(|| {
                "no mesh genesis found — `ingest` copies history out of a local node, so it \
                 needs that node's home (pass --home, or set MESHWARE_HOME)"
                    .to_string()
            })?;
            let archive = Archive::for_mesh(root, mesh_id).map_err(|e| e.to_string())?;
            let (jobs, audit) = ingest(&archive, home.clone()).await?;
            println!("archived {jobs} jobs, {audit} audit entries");
            if !no_compact {
                for table in Table::ALL {
                    let done = archive.compact(table).map_err(|e| e.to_string())?;
                    if !done.is_empty() {
                        println!("compacted {}: {}", table.sql_name(), done.join(", "));
                    }
                }
            }
            // After compaction, so the bucket receives merged parts rather
            // than the fragments compaction was about to replace — and so
            // a re-push does not have to reconcile the two.
            if let Some(url) = bucket {
                // The lock window defaults to *this mesh's* audit retention
                // rather than to a number invented here — a WORM copy that
                // expired before the policy said it could would be the one
                // failure mode worse than not having one.
                let days = match worm_days {
                    Some(d) => Some(d),
                    None if worm => audit_window(home.as_ref()).await,
                    None => None,
                };
                push_to_bucket(
                    root,
                    &url,
                    endpoint,
                    region,
                    credential,
                    worm,
                    days,
                    home.as_ref(),
                )
                .await?;
            }
            Ok(())
        }
        Command::Verify { all } => {
            let proofs = archive.audit_proofs().await.map_err(|e| e.to_string())?;
            if proofs.is_empty() {
                println!("no audit rows in this archive — nothing to verify");
                return Ok(());
            }
            let mut bad = 0usize;
            for proof in &proofs {
                let ok = verify_proof(proof);
                if !ok {
                    bad += 1;
                }
                if all || !ok {
                    println!(
                        "{}  {}  {}",
                        if ok { "ok    " } else { "FAILED" },
                        proof.audit_id,
                        proof.node
                    );
                }
            }
            println!(
                "{} audit row(s) · {} verified · {bad} failed",
                proofs.len(),
                proofs.len() - bad
            );
            if bad > 0 {
                return Err(format!(
                    "{bad} audit row(s) do not verify against the key that signed them"
                ));
            }
            Ok(())
        }
        Command::Push {
            bucket,
            endpoint,
            region,
            credential,
            worm,
            worm_days,
        } => {
            push_to_bucket(
                root, &bucket, endpoint, region, credential, worm, worm_days, None,
            )
            .await
        }
        Command::Compact => {
            for table in Table::ALL {
                let done = archive.compact(table).map_err(|e| e.to_string())?;
                if done.is_empty() {
                    println!("{}: nothing to compact", table.sql_name());
                } else {
                    println!("{}: compacted {}", table.sql_name(), done.join(", "));
                }
            }
            Ok(())
        }
        Command::Prune {
            home,
            days,
            dry_run,
        } => {
            let days = match days {
                Some(d) => d,
                None => archive_window(home.as_ref()).await.ok_or_else(|| {
                    "could not read this mesh's `archive` window from the node; pass --days"
                        .to_string()
                })?,
            };
            if days == 0 {
                // `forever`. Refused rather than treated as "delete
                // everything", which is what a zero-day cutoff would mean
                // if it were computed instead of caught here.
                println!("archive retention is `forever` — nothing is pruned");
                return Ok(());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_micros() as u64;
            let cutoff = mesh_archive::partition_of(
                now.saturating_sub(u64::from(days) * 86_400 * 1_000_000),
            );
            println!("archive retention: {days}d — keeping date>={cutoff}");
            for table in Table::ALL {
                let gone = if dry_run {
                    archive
                        .partitions_before(table, &cutoff)
                        .map_err(|e| e.to_string())?
                } else {
                    archive.prune(table, &cutoff).map_err(|e| e.to_string())?
                };
                let verb = if dry_run { "would prune" } else { "pruned" };
                if gone.is_empty() {
                    println!("{}: nothing to prune", table.sql_name());
                } else {
                    println!("{}: {verb} {}", table.sql_name(), gone.join(", "));
                }
            }
            Ok(())
        }
        Command::Query { sql: None, .. } => {
            println!("Example queries — pass one as an argument:\n");
            for (title, sql) in Archive::examples() {
                println!("  # {title}");
                println!("  {sql}\n");
            }
            Ok(())
        }
        Command::Query {
            sql: Some(sql),
            json,
        } => {
            if !archive.exists() {
                return Err(format!("no archive at {}", root.display()));
            }
            // Only matters for a format-1 archive, where it is the value
            // `mesh_id` projects to. Absent is fine and reads as null.
            let archive = match local_mesh_id(None) {
                Some(id) => archive.reading_as_mesh(id),
                None => archive,
            };
            let result = archive.sql(&sql).await.map_err(|e| e.to_string())?;
            if json {
                let out =
                    serde_json::to_string_pretty(&result.to_json()).map_err(|e| e.to_string())?;
                println!("{out}");
            } else {
                print_table(&result);
            }
            if result.truncated {
                eprintln!(
                    "\n(stopped at {} rows — add a LIMIT or narrow the query)",
                    result.rows.len()
                );
            }
            Ok(())
        }
    }
}

/// The oldest and newest instants in a page, and how many rows it held.
///
/// The page arrives newest-first, so this is `(last, first, len)` — spelled
/// out rather than indexed so the direction is stated once, here, instead of
/// being re-derived at each use.
fn bounds(times: impl Iterator<Item = u64>) -> (Option<u64>, Option<u64>, usize) {
    let mut newest: Option<u64> = None;
    let mut oldest: Option<u64> = None;
    let mut count = 0usize;
    for t in times {
        if newest.is_none() {
            newest = Some(t);
        }
        oldest = Some(t);
        count += 1;
    }
    (oldest, newest, count)
}

/// What the descending sweep does with one probed page (**H-7**).
enum Step {
    /// Archive this `(since, until)` window; continue from `next`, or stop
    /// when it is `None`.
    Take {
        window: (u64, u64),
        next: Option<u64>,
    },
    /// Nothing left.
    Done,
}

/// Decide the next window from one probe. Pure, so the ordering rule that
/// this whole finding turned on can be tested without a node.
///
/// `probe` is `(oldest, newest, count)` of a page fetched with
/// `until_micros = until`.
fn next_window(until: Option<u64>, probe: (Option<u64>, Option<u64>, usize), page: usize) -> Step {
    let (Some(oldest), Some(newest), count) = probe else {
        return Step::Done;
    };
    let window = (oldest, until.unwrap_or(newest));
    if count < page {
        // A short page is the last one.
        return Step::Take { window, next: None };
    }
    // Inclusive, so rows sharing `oldest`'s microsecond that did not fit
    // this page are picked up by the next one. Duplicates across the seam
    // are free — the archive skips anything at or below its watermark.
    let next = if until == Some(oldest) {
        // A whole page inside one microsecond: step below it or the walk
        // cannot advance. Vanishingly unlikely at PAGE = 2000, and losing a
        // tie here is better than looping forever.
        if oldest == 0 {
            return Step::Take { window, next: None };
        }
        Some(oldest - 1)
    } else {
        Some(oldest)
    };
    Step::Take { window, next }
}

/// Ask the node for one page of job history.
async fn fetch_page(
    socket: &std::path::Path,
    user: &mesh_core::identity::Keypair,
    query: Box<mesh_proto::HistoryQuery>,
) -> Result<Vec<mesh_proto::HistoryJobRow>, String> {
    use mesh_proto::{IpcRequest, IpcResponse};
    let resp = mesh_core::ipc::request(socket, user, IpcRequest::HistoryJobs(query))
        .await
        .map_err(|e| format!("talking to the node: {e}"))?;
    match resp {
        IpcResponse::HistoryJobs(page) => Ok(page.rows),
        IpcResponse::Error { message } => Err(message),
        IpcResponse::Denied { message } => Err(format!("permission denied: {message}")),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Page the local node's history index into the archive.
///
/// Paged rather than fetched whole: a first pass on a long-lived mesh is
/// hundreds of thousands of rows, and one IPC frame that size would spike
/// memory on both ends to save a few round trips.
async fn ingest(archive: &Archive, home: Option<PathBuf>) -> Result<(u64, u64), String> {
    use mesh_core::config::{NodeConfig, Overrides};
    use mesh_proto::{HistoryQuery, IpcRequest, IpcResponse};

    const PAGE: u32 = 2000;

    let config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: home,
            ..Default::default()
        },
    )
    .map_err(|e| format!("resolving the node's home: {e}"))?;
    let socket = config.socket_path();
    if !socket.exists() {
        return Err(format!(
            "no node is listening at {} — start one with `meshnode up`, \
             or pass --home if it keeps its data somewhere else",
            socket.display()
        ));
    }
    let user = mesh_core::identity::load_cli_user(&config.data_dir, None)
        .map_err(|e| format!("loading the user identity: {e}"))?;
    let now = mesh_proto::now_micros();

    let mut jobs_total = 0;
    let mut audit_total = 0;

    // -- RETIRED 2026-09-20: the usage-window pass -------------------------
    //
    // It paged `IpcRequest::UsageRows`, which became a payload-free
    // tombstone when quotas and the usage report were removed. The request
    // it sent could no longer be constructed, so this binary had not
    // compiled since — `required-features = ["cli"]` keeps it out of
    // `cargo check --workspace --all-targets`, which is why nothing said
    // so. `Archive::ingest_usage` and the `usage` table stay: existing
    // `usage/` partitions are still queryable, and usage rows still leave a
    // node through a sink.

    // The audit trail, with its proofs (M31 B4). Paged by instant like
    // usage rather than by offset, because entries are only ever appended
    // and an offset into a growing set skips rows.
    //
    // Its own loop and not part of the job sweep below: that pages a
    // *projection* through `HistoryQuery`, and this pages signed envelopes
    // through a request that exists because the projection cannot carry
    // them.
    {
        let mut since = archive
            .manifest()
            .map(|m| m.audit_watermark_micros)
            .unwrap_or(0);
        loop {
            let resp = mesh_core::ipc::request(
                &socket,
                &user,
                IpcRequest::AuditRows {
                    since_micros: since,
                    limit: PAGE,
                },
            )
            .await
            .map_err(|e| format!("talking to the node: {e}"))?;
            let rows = match resp {
                IpcResponse::AuditRows(rows) => rows,
                // A node too old to answer has no audit table to fill, and
                // that is not a failure of this run — the rest of the
                // ingest is still worth doing.
                IpcResponse::Error { .. } => break,
                IpcResponse::Denied { message } => {
                    return Err(format!(
                        "permission denied reading the audit trail: {message} — archiving it \
                         needs the auditor role"
                    ));
                }
                other => return Err(format!("unexpected response: {other:?}")),
            };
            if rows.is_empty() {
                break;
            }
            let last = rows
                .iter()
                .map(|r| r.entry.at_micros)
                .max()
                .unwrap_or(since);
            let report = archive.ingest_audit(&rows).map_err(|e| e.to_string())?;
            audit_total += report.audit_written;
            if report.audit_refused > 0 {
                // Loud, and not a statistic: a row that does not bind its
                // own signed bytes is corruption or tampering.
                eprintln!(
                    "warning: {} audit row(s) failed their own signature binding and were \
                     not archived",
                    report.audit_refused
                );
            }
            if last <= since {
                break;
            }
            since = last + 1;
        }
    }

    // Jobs are the one table this tool pages over IPC, and since the run
    // index was retired on 2026-09-20 there is no second one to sweep.
    // Facts and results are written by Director, which holds them already;
    // paging them here would be a match arm that cannot be written rather
    // than a feature that is missing.
    //
    // -- pass 1: learn the page boundaries, newest first ------------------
    //
    // **Keyset, not offset** (H-7). The index cannot seek to an offset:
    // it answers one by decoding and counting rows from the newest
    // until it has skipped that many, so page *k* re-walked `k × PAGE`
    // rows to reach the ones it wanted. Total work was N²/(2·PAGE) —
    // 200 000 jobs meant ~10 million decodes instead of 200 000, and it
    // held a read transaction across every one of them.
    //
    // A time bound costs nothing: the key is time-prefixed, so
    // `until_micros` starts the scan in the right place instead of
    // counting up to it. Only the window edges are kept here — a
    // handful of integers per page — never the rows.
    let mut windows: Vec<(u64, u64)> = Vec::new();
    let mut until: Option<u64> = None;
    loop {
        let query = Box::new(HistoryQuery {
            limit: PAGE,
            until_micros: until,
            ..Default::default()
        });
        let rows = fetch_page(&socket, &user, query).await?;
        let probe = bounds(rows.iter().map(|r| r.sort_micros()));
        match next_window(until, probe, PAGE as usize) {
            Step::Take { window, next } => {
                windows.push(window);
                match next {
                    Some(u) => until = Some(u),
                    None => break,
                }
            }
            Step::Done => break,
        }
    }

    // -- pass 2: archive them, oldest first -------------------------------
    //
    // **The order is the correctness fix, and it is not optional.**
    // `Archive::ingest` skips every row at or below the watermark it
    // held when the call began, then advances the watermark to the
    // newest row it wrote. Fed newest-first, the first page moved the
    // watermark to the newest record in the mesh and *every older page
    // after it was discarded as already-archived* — so a backfill kept
    // the newest 2000 rows and silently dropped the rest, reporting
    // success. Feeding it oldest-first is what the watermark has always
    // assumed. Pinned by `a_backfill_paged_oldest_first_archives_every_page`.
    for (since, until) in windows.into_iter().rev() {
        let query = Box::new(HistoryQuery {
            limit: PAGE,
            since_micros: Some(since),
            until_micros: Some(until),
            ..Default::default()
        });
        let rows = fetch_page(&socket, &user, query).await?;
        let report = archive.ingest(&rows, now).map_err(|e| e.to_string())?;
        jobs_total += report.jobs_written;
    }

    Ok((jobs_total, audit_total))
}

/// This mesh's `archive` retention window, in days, for `prune` (ENG-19).
///
/// Asked of the node rather than configured here, because retention is one
/// mesh-wide policy and a second copy of it in a second place is a second
/// answer waiting to be wrong. `None` when the node cannot be reached, and
/// `prune` then refuses rather than guessing — deleting history on a
/// default is not a thing this should do.
async fn archive_window(home: Option<&PathBuf>) -> Option<u32> {
    retention_days(home, mesh_proto::RetentionTable::Archive).await
}

/// This mesh's `audit` retention window, in days, for `--worm`.
///
/// Best-effort: a node that is down, or a build too old to answer, simply
/// leaves the lock at the floor. A year is never wrong, only sometimes
/// shorter than an operator asked for — and the alternative, refusing to
/// push because the window could not be read, would be worse.
async fn audit_window(home: Option<&PathBuf>) -> Option<u32> {
    retention_days(home, mesh_proto::RetentionTable::Audit).await
}

/// One table's window from the node's own `retention show`.
///
/// One function for both callers: retention is a single mesh-wide policy,
/// and two ways of reading it is two answers waiting to disagree.
async fn retention_days(home: Option<&PathBuf>, table: mesh_proto::RetentionTable) -> Option<u32> {
    use mesh_core::config::{NodeConfig, Overrides};
    let config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: home.cloned(),
            ..Default::default()
        },
    )
    .ok()?;
    let user = mesh_core::identity::load_cli_user(&config.data_dir, None).ok()?;
    match mesh_core::ipc::request(
        &config.socket_path(),
        &user,
        mesh_proto::IpcRequest::RetentionShow,
    )
    .await
    {
        Ok(mesh_proto::IpcResponse::Retention(view)) => Some(view.policy.window(table).days),
        _ => None,
    }
}

/// Resolve `{{secret:name}}` into an S3 key pair (M31 B4 item 2).
///
/// The secret's value is `ACCESS_KEY_ID:SECRET_ACCESS_KEY` — one secret
/// rather than two, because they are one credential and rotating half of a
/// key pair is not a thing anybody does.
///
/// Unsealed with the **node** key, which is why this binary has to run as
/// the same user as the daemon: a sealed secret is sealed to nodes. The
/// shipped systemd unit does exactly that, and it is the reason the unit
/// names `User=meshnode` rather than leaving it to whoever ran it.
async fn resolve_bucket_credential(
    reference: &str,
    home: Option<&PathBuf>,
) -> Result<(Option<String>, Option<String>), String> {
    use mesh_core::config::{NodeConfig, Overrides};

    let config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: home.cloned(),
            ..Default::default()
        },
    )
    .map_err(|e| format!("resolving the node's home: {e}"))?;
    let genesis = mesh_core::genesis::Genesis::load(&config.data_dir)
        .map_err(|e| e.to_string())?
        .ok_or("no mesh genesis here — a `{{secret:…}}` reference needs the node's home")?;
    let identity = mesh_core::identity::Identity::load_or_generate(&config.data_dir)
        .map_err(|e| format!("loading the node identity: {e}"))?;
    let engine =
        mesh_core::state::StateEngine::open(&config.data_dir, identity.node.clone(), genesis)
            .map_err(|e| format!("opening the state store: {e}"))?;

    let dir = config.layout().archive_dir().join("secrets");
    let resolved = mesh_core::secrets::resolve_env(
        &engine,
        &identity.node,
        &config.secret_providers,
        vec![("BUCKET".to_string(), reference.to_string())],
        &dir,
        // The node's own use, not a user's: this is a bucket credential an
        // admin configured, and SEC-7's node half still applies to it.
        // (This argument was added to `resolve_env` and never threaded
        // through here, which left this binary uncompilable behind its
        // feature flag — `--all-features` is the only thing that builds it.)
        None,
    )
    .await
    .map_err(|e| format!("resolving {reference}: {e}"))?;

    let value = resolved
        .env
        .iter()
        .find(|(k, _)| k == "BUCKET")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    match value.split_once(':') {
        Some((access, secret)) if !access.is_empty() && !secret.is_empty() => {
            Ok((Some(access.to_string()), Some(secret.to_string())))
        }
        _ => Err(format!(
            "the secret behind {reference} is not `ACCESS_KEY_ID:SECRET_ACCESS_KEY` — one \
             secret holds both halves, because they are one credential"
        )),
    }
}

/// Check one archived row against the key carried in its own `node`
/// column.
///
/// **Forty lines in any language**, and this is what they look like in
/// Rust. Nothing here reads a manifest, consults a mesh, or knows what an
/// op is: a `NodeId` is an Ed25519 public key, `signing_bytes` is what was
/// signed, and `signature` is the signature. That is the whole contract,
/// and it is the reason `signing_bytes` is carried rather than derived.
fn verify_proof(proof: &mesh_archive::AuditProof) -> bool {
    let Ok(node) = mesh_proto::NodeId::parse(&proof.node) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(proof.signature.as_slice()) else {
        return false;
    };
    mesh_core::identity::verify_signature(&node.0, &proof.signing_bytes, &sig)
}

/// Replicate the archive directory to a bucket (M31 B4).
///
/// Credentials come from the environment — `AWS_ACCESS_KEY_ID` and friends,
/// an instance profile, IRSA, a developer's `AWS_PROFILE`. **Never from
/// argv**: the same rule a sink credential follows, for the same reason,
/// and the reason `--access-key` is not a flag here.
#[allow(clippy::too_many_arguments)]
async fn push_to_bucket(
    root: &Path,
    url: &str,
    endpoint: Option<String>,
    region: Option<String>,
    credential: Option<String>,
    worm: bool,
    worm_days: Option<u32>,
    home: Option<&PathBuf>,
) -> Result<(), String> {
    use mesh_archive::bucket::{BucketTarget, lock_days, push};

    let target = BucketTarget::parse(url)
        .map_err(|e| e.to_string())?
        .with_endpoint(endpoint)
        .with_region(region);
    // Plain HTTP is for a MinIO on somebody's own network, which is the
    // ordinary development and on-premises case; over the open internet it
    // would put a mesh's whole history and its credential on the wire.
    let allow_http = target
        .endpoint
        .as_deref()
        .is_some_and(|e| e.starts_with("http://"));
    // Resolved on this machine, at connect time, exactly as a sink
    // credential is (C6). `None` leaves the environment to answer, which is
    // what every AWS tool already does.
    let (access, secret) = match &credential {
        Some(reference) => resolve_bucket_credential(reference, home).await?,
        None => (None, None),
    };
    let store = target
        .client(access.as_deref(), secret.as_deref(), allow_http, None)
        .map_err(|e| e.to_string())?;

    // A second client, and only when `--worm` was asked for. The Object
    // Lock headers ride on every request it makes, so an audit part cannot
    // be written unlocked by forgetting a flag at a call site — the type
    // carries the policy.
    let days = worm.then(|| lock_days(worm_days.unwrap_or(0)));
    let locked = match days {
        Some(d) => Some(
            target
                .client(access.as_deref(), secret.as_deref(), allow_http, Some(d))
                .map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    let report = push(root, &target, &store, locked.as_ref().zip(days))
        .await
        .map_err(|e| e.to_string())?;

    println!(
        "pushed {} object(s), {} already there, {:.1} MB",
        report.uploaded,
        report.already_there,
        report.bytes as f64 / (1024.0 * 1024.0)
    );
    if report.locked > 0 {
        println!(
            "{} audit object(s) written under Object Lock, compliance mode, for {} days",
            report.locked,
            days.unwrap_or(0)
        );
    } else if worm {
        // Asked for and not applied means there were no audit objects to
        // lock — worth saying, because silence would read as success.
        println!("no audit objects to lock — nothing has been audited into this archive yet");
    }
    Ok(())
}

/// Columns sized to their contents. Padding every column to a fixed width
/// wastes half the terminal on a `run_id` and truncates the `detail` that
/// somebody actually opened the archive to read.
fn print_table(result: &mesh_archive::QueryResult) {
    if result.rows.is_empty() {
        println!("(no rows)");
        return;
    }
    let mut widths: Vec<usize> = result.columns.iter().map(|c| c.chars().count()).collect();
    for row in &result.rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count()).min(60);
            }
        }
    }

    let header: Vec<String> = result
        .columns
        .iter()
        .zip(&widths)
        .map(|(c, w)| format!("{c:<w$}"))
        .collect();
    println!("{}", header.join("  "));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );

    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                let cell: String = if cell.chars().count() > *w {
                    cell.chars()
                        .take(w.saturating_sub(1))
                        .chain(['…'])
                        .collect()
                } else {
                    cell.clone()
                };
                format!("{cell:<w$}")
            })
            .collect();
        println!("{}", cells.join("  "));
    }
    println!("\n{} rows", result.rows.len());
}

#[cfg(test)]
mod backfill_paging {
    use super::*;

    /// A synthetic index: `times` newest-first, answering the same way
    /// `query_jobs` does — the newest `page` rows at or below `until`.
    fn probe(times: &[u64], until: Option<u64>, page: usize) -> (Option<u64>, Option<u64>, usize) {
        let visible: Vec<u64> = times
            .iter()
            .copied()
            .filter(|t| until.is_none_or(|u| *t <= u))
            .take(page)
            .collect();
        bounds(visible.into_iter())
    }

    /// Walk the whole index the way `ingest` does and return the windows in
    /// the order they will be archived.
    fn plan(times: &[u64], page: usize) -> Vec<(u64, u64)> {
        let mut windows = Vec::new();
        let mut until: Option<u64> = None;
        while let Step::Take { window, next } = next_window(until, probe(times, until, page), page)
        {
            windows.push(window);
            match next {
                Some(u) => until = Some(u),
                None => break,
            }
        }
        windows.reverse(); // pass 2 archives oldest-first
        windows
    }

    /// **The finding, as a test.** Every window must be archived in
    /// ascending order, because `Archive::ingest`'s watermark discards
    /// anything at or below what it has already seen.
    #[test]
    fn windows_are_archived_oldest_first() {
        let times: Vec<u64> = (1..=10).rev().map(|n| n * 100).collect(); // 1000..100
        let windows = plan(&times, 3);
        assert!(
            windows.windows(2).all(|w| w[0].0 <= w[1].0),
            "windows must ascend, got {windows:?}"
        );
        assert_eq!(windows.first().map(|w| w.0), Some(100), "{windows:?}");
    }

    /// Every row in the index falls inside some window — nothing is skipped
    /// between pages.
    #[test]
    fn every_row_is_covered_by_a_window() {
        let times: Vec<u64> = (1..=10).rev().map(|n| n * 100).collect();
        let windows = plan(&times, 3);
        for t in &times {
            assert!(
                windows
                    .iter()
                    .any(|(since, until)| t >= since && t <= until),
                "{t} fell between windows {windows:?}"
            );
        }
    }

    /// Ties at a page boundary are carried into the next window rather than
    /// dropped — the reason the boundary is inclusive.
    #[test]
    fn rows_sharing_a_boundary_instant_are_not_lost() {
        // Three rows at 300 straddle a page of size 2.
        let times = vec![500, 400, 300, 300, 300, 200];
        let windows = plan(&times, 2);
        for t in &times {
            assert!(
                windows
                    .iter()
                    .any(|(since, until)| t >= since && t <= until),
                "{t} fell between windows {windows:?}"
            );
        }
    }

    /// A page entirely inside one microsecond must still terminate.
    #[test]
    fn a_page_of_identical_instants_still_advances() {
        let times = vec![7, 7, 7, 7, 7];
        let windows = plan(&times, 2);
        assert!(!windows.is_empty());
    }

    #[test]
    fn an_empty_index_plans_nothing() {
        assert!(plan(&[], 3).is_empty());
    }
}
