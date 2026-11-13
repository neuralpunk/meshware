//! Human-readable table rendering for CLI reads. JSON output goes through
//! serde; tables are hand-formatted for terminal legibility.

use anyhow::Result;

use crate::term;
use mesh_proto::{
    AuditEntry, BundleRow, GrantRow, IdentityInfo, JobRow, Liveness, NodeRow, ObjectRow,
    PingResult, ProviderRow, SecretRow, ServiceRow, SessionPolicyRow, SessionRow, SourceRow,
    StatusInfo, TopRow, Totals, UserRow, WhoAmIInfo,
};

/// Byte counts for callers outside this module.
pub fn bytes(n: u64) -> String {
    human_bytes(n)
}

/// A table for the terminal.
///
/// No borders, two spaces between columns, and the terminal's width
/// respected: a long command wraps inside its cell rather than pushing the
/// row onto the next line, and a node named in Japanese lines up with one
/// named in ASCII. Off a terminal — a pipe, a log — nothing wraps and
/// nothing is trimmed, which is what a pipe wants. The right-aligned
/// columns are the numeric ones, named by index.
///
/// Coloured by what each column means, so every table in the CLI reads
/// the same way without its caller choosing a colour: the header and the
/// detail (ids, times, `-`) recede, the row's name is bold, and a state
/// or status glyph is in the colour of what it says — see
/// [`crate::term::tone_of`]. Plain when colour is off, byte for byte what
/// it was.
fn table(header: &[&str], right: &[usize]) -> Grid {
    use comfy_table::{Cell, CellAlignment, ContentArrangement, Table, presets};
    let paint = crate::term::caps().out.color;
    let mut t = Table::new();
    t.load_preset(presets::NOTHING);
    t.set_content_arrangement(ContentArrangement::Dynamic);
    if paint {
        // `--color always` into a pipe: comfy-table would otherwise decide
        // for itself, from the tty, and drop what was asked for.
        t.enforce_styling();
        t.style_text_only();
        t.set_header(
            header
                .iter()
                .map(|h| Cell::new(h).add_attribute(comfy_table::Attribute::Dim)),
        );
    } else {
        t.set_header(header);
    }
    for (i, column) in t.column_iter_mut().enumerate() {
        column.set_padding((0, 2));
        if right.contains(&i) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    let kinds = header
        .iter()
        .enumerate()
        .map(|(i, h)| Column::of(i, h))
        .collect();
    Grid { t, kinds, paint }
}

/// [`table`]'s table: comfy-table, and what each column holds.
struct Grid {
    t: comfy_table::Table,
    kinds: Vec<Column>,
    paint: bool,
}

/// What a column holds, from its header — which is all a reader has to go
/// on too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Plain,
    /// `●`, `⊘`: a status drawn as a glyph.
    Glyph,
    /// A state word.
    State,
    /// What the row is: the first column, or one headed NAME.
    Name,
    /// When: ages, dates, durations.
    Time,
}

impl Column {
    fn of(index: usize, header: &str) -> Column {
        match header {
            "ST" | "" => Column::Glyph,
            "STATE" | "STATUS" | "HEALTH" | "OUTCOME" | "RESULT" | "VERDICT" => Column::State,
            "AGE" | "ADDED" | "JOINED" | "CREATED" | "EXPIRES" | "WHEN" | "UPDATED" | "STARTED"
            | "FINISHED" | "LAST" | "SEEN" | "SINCE" | "TOOK" | "UPTIME" | "LAST SEEN" | "NEXT" => {
                Column::Time
            }
            "NAME" => Column::Name,
            _ if index == 0 => Column::Name,
            _ => Column::Plain,
        }
    }
}

impl Grid {
    fn add_row(&mut self, row: Vec<String>) {
        if !self.paint {
            self.t.add_row(row);
            return;
        }
        use crate::term::{glyph_tone, looks_like_id, tone_of};
        use comfy_table::{Attribute, Cell, Color};
        let cells = row.into_iter().enumerate().map(|(i, text)| {
            let kind = self.kinds.get(i).copied().unwrap_or(Column::Plain);
            let tone = if text.trim().is_empty() || text.trim() == "-" || looks_like_id(&text) {
                Tone::Quiet
            } else {
                match kind {
                    Column::Glyph => glyph_tone(&text).unwrap_or(Tone::Plain),
                    Column::State => tone_of(&text),
                    Column::Name => Tone::Strong,
                    Column::Time => Tone::Quiet,
                    Column::Plain => Tone::Plain,
                }
            };
            let cell = Cell::new(text);
            match tone {
                Tone::Plain => cell,
                Tone::Strong => cell.add_attribute(Attribute::Bold),
                Tone::Quiet => cell.add_attribute(Attribute::Dim),
                Tone::Good => cell.fg(Color::DarkGreen),
                Tone::Bad => cell.fg(Color::DarkRed),
                Tone::Unknown => cell.fg(Color::DarkYellow),
                Tone::Accent => cell.fg(Color::DarkCyan),
            }
        });
        self.t.add_row(cells.collect::<Vec<Cell>>());
    }
}

impl std::fmt::Display for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.t.fmt(f)
    }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn micros_ago(now: u64, then: u64) -> String {
    if then == 0 || then > now {
        return "-".into();
    }
    let secs = (now - then) / 1_000_000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// "40 minutes", for a duration a person is being asked to judge.
///
/// The mirror of [`micros_ago`], which answers *when*; this answers *for
/// how long*, which is the question a canary ring poses.
pub fn since(now: u64, then: u64) -> String {
    if then == 0 || then > now {
        return "just now".into();
    }
    let secs = (now - then) / 1_000_000;
    if secs < 90 {
        format!("{secs} seconds")
    } else if secs < 5400 {
        format!("{} minutes", secs / 60)
    } else if secs < 172_800 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

fn liveness_glyph(l: Liveness) -> &'static str {
    match l {
        Liveness::Alive => "●",
        Liveness::Suspect => "◐",
        Liveness::Dead => "○",
    }
}

/// `meshnode status`, and with `--totals` the mesh's spec sheet under it.
///
/// The strip is a separate argument rather than a field on `StatusInfo`
/// because it is a different *subject*: everything above it is this
/// daemon — its node, its slots, its uptime — and the strip is every
/// machine in the mesh. Printing them in one block is fine; summing the
/// mesh on every `status` call, including the one `/healthz` makes, is not.
pub fn print_status(s: &StatusInfo, totals: Option<&Totals>, json: bool) -> Result<()> {
    if json {
        // `--totals --output json` is an object with two members rather
        // than a `StatusInfo` with extra keys: a consumer parsing status
        // into a struct should not have its shape change because somebody
        // added a flag, and the two halves are about different things.
        match totals {
            Some(t) => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": s,
                    "totals": t,
                    "strip": t.strip(),
                    "coverage": t.coverage(),
                }))?
            ),
            None => println!("{}", serde_json::to_string_pretty(s)?),
        }
        return Ok(());
    }
    println!(
        "{} {}",
        term::out().bold("Meshware Node"),
        term::out().dim(&s.version)
    );
    println!(
        "  {}      {}  ({})",
        term::label("node"),
        s.node_id.short(),
        s.node_name
    );
    println!(
        "  {}      {}  ({})",
        term::label("mesh"),
        s.mesh_name.as_deref().unwrap_or("-"),
        s.mesh_id
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("  {}    {}", term::label("listen"), s.listen_addr);
    // Only when it is somebody else. On the founder's own machine the line
    // says nothing the line above it did not, and the one time it matters
    // — an address repair, which only the founder may author — the
    // question being asked is "which *other* node do I run this on".
    if s.founder_node != s.node_id {
        println!("  {}   {}", term::label("founder"), s.founder_node.short());
    }
    println!(
        "  {}   {} alive / {} total",
        term::label("members"),
        s.members_alive,
        s.members_total
    );
    match &s.quiet {
        // Both numbers when a window is in force, because "4 running, 0
        // queued (16 slots)" on a node that will only start four reads as
        // a mesh that has stopped working.
        Some(q) if q.pct.is_some() => println!(
            "  {}      {} running, {} queued ({} of {} slots — quiet hours)",
            term::label("jobs"),
            s.jobs_running,
            s.jobs_queued,
            q.effective_slots,
            s.slots
        ),
        _ => println!(
            "  {}      {} running, {} queued ({} slots)",
            term::label("jobs"),
            s.jobs_running,
            s.jobs_queued,
            s.slots
        ),
    }
    if let Some(q) = &s.quiet {
        let now = match (q.pct, q.until.as_deref()) {
            (Some(pct), Some(until)) => format!("{pct}% until {until}"),
            (Some(pct), None) => format!("{pct}%"),
            (None, Some(until)) => format!("full until {until}"),
            (None, None) => "full".to_string(),
        };
        println!(
            "  {}     {}  [{}]  now: {now}",
            term::label("quiet"),
            q.windows,
            q.zone
        );
    }
    match (&s.drain, s.draining) {
        (Some(d), _) => println!(
            "  {}  until {}{} — running jobs finish, new work goes elsewhere",
            term::label("draining"),
            mesh_core::localtime::describe_instant(d.until_micros),
            if d.reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", d.reason)
            }
        ),
        (None, true) => println!(
            "  {}  for good — finishing accepted work, then exiting",
            term::label("draining")
        ),
        (None, false) => {}
    }
    // N6 §12 — what this node's own dispatches cost. MASTERPLAN bet #3
    // calls this "the product", and until N6 it was the one claim the
    // product made that an operator could not check.
    //
    // Bucket edges, so the line reads as a bound rather than as a
    // measurement it does not have: `under 5ms` is true, `3.7ms` would be
    // invented.
    if let Some(d) = &s.dispatch {
        let edge = |v: Option<f64>| match v {
            Some(secs) if secs < 1.0 => format!("<{}ms", (secs * 1000.0).round() as u64),
            Some(secs) => format!("<{secs}s"),
            None => "-".to_string(),
        };
        println!(
            "  {}  p50 {} · p95 {}  ({} placed)",
            term::label("dispatch"),
            edge(d.p50_secs),
            edge(d.p95_secs),
            d.count
        );
    }
    // N6 §12. Printed only when it is not the default, on the rule the
    // founder line above it follows: a status screen that states every
    // default is a screen nobody reads to the bottom. `pack` is the answer
    // that surprises somebody looking at where their jobs went.
    if s.placement != mesh_proto::FitPolicy::default() {
        println!(
            "  placement {} — this node fills a machine before starting the next",
            s.placement.as_str()
        );
    }
    // T4. One line, always — unlike `placement` above, this is not a
    // default anybody can assume: it is what this particular machine has
    // installed, and it is the answer to "why did my job not run here".
    // `meshnode runtimes` is the table; this is the shape of it.
    if !s.runtimes.is_empty() {
        println!(
            "  {}  {}",
            term::label("runtimes"),
            s.runtimes
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "  {}  {}.{}",
        term::label("protocol"),
        s.proto_major,
        s.proto_minor
    );
    println!("  {}    {}s", term::label("uptime"), s.uptime_secs);
    if let Some(w) = &s.wan {
        let mut line = w.reachability.clone();
        if let Some(r) = &w.reflexive_addr {
            line.push_str(&format!(" (seen as {r})"));
        }
        if !w.advertise.is_empty() {
            line.push_str(&format!(", advertises {}", w.advertise.join(",")));
        }
        if w.relay_server {
            line.push_str(&format!(", relay for {} client(s)", w.relay_clients));
        }
        if !w.relays.is_empty() {
            line.push_str(&format!(", via relays {}", w.relays.join(",")));
        }
        println!("  {}       {line}", term::label("wan"));
    }
    if s.draining {
        println!(
            "  {}     {} — finishing accepted work, refusing new",
            term::label("state"),
            term::out().warn("DRAINING")
        );
    }
    if let Some(t) = totals {
        println!("  {}    {}", term::label("totals"), t.strip());
        // Coverage sits under the figures it qualifies, indented to their
        // column, and appears only when they are a floor (D-L2.2).
        if let Some(coverage) = t.coverage() {
            println!("            {coverage}");
        }
    }
    Ok(())
}

/// `meshnode adopt`: one row per host, and how many did not make it.
///
/// The confirmation code is in the table on purpose. It is the evidence
/// that the exchange happened and that two independently-derived codes
/// agreed — the thing a person would have compared by eye — so it belongs
/// in the transcript somebody reads back afterwards, not swallowed.
pub fn print_adopted(outcomes: &[crate::adopt::Outcome], json: bool) -> Result<usize> {
    let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
    if json {
        let rows: Vec<serde_json::Value> = outcomes
            .iter()
            .map(|o| match &o.result {
                Ok(a) => serde_json::json!({
                    "target": o.target,
                    "ok": true,
                    "node_name": a.node_name,
                    "node_id": a.node_id,
                    "code": a.code,
                    "installed": !a.already_installed,
                    "scope": a.scope,
                    "service": a.service,
                    "note": a.note,
                }),
                Err(e) => serde_json::json!({
                    "target": o.target,
                    "ok": false,
                    "error": e.to_string(),
                }),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(failed);
    }
    if outcomes.is_empty() {
        println!("no targets");
        return Ok(0);
    }
    let mut t = table(&["TARGET", "STATE", "DETAIL"], &[]);
    for o in outcomes {
        let (state, detail) = match &o.result {
            Ok(a) if a.node_id.is_empty() => (
                "dry-run",
                format!(
                    "would {}, {} install, {}{}",
                    if a.already_installed {
                        "pair (binary already current)"
                    } else {
                        "install and pair"
                    },
                    a.scope,
                    a.service,
                    a.note
                        .as_deref()
                        .map(|n| format!("\n{n}"))
                        .unwrap_or_default(),
                ),
            ),
            Ok(a) => (
                "adopted",
                format!(
                    "{} ({}) · code {} · {}{}{}",
                    a.node_name,
                    a.node_id.chars().take(12).collect::<String>(),
                    mesh_core::pairing::format_code(&a.code),
                    a.service,
                    if a.already_installed {
                        " · binary already current"
                    } else {
                        ""
                    },
                    a.note
                        .as_deref()
                        .map(|n| format!("\n{n}"))
                        .unwrap_or_default(),
                ),
            ),
            // The whole error chain, one cause per line: "ssh failed"
            // without the reason underneath is the most common useless
            // thing a tool like this can print.
            Err(e) => (
                "FAILED",
                e.chain()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        t.add_row(vec![o.target.clone(), state.to_string(), detail]);
    }
    println!("{t}");
    if failed > 0 {
        println!();
        println!("{failed} of {} host(s) were not adopted.", outcomes.len());
    }
    Ok(failed)
}

/// `meshnode top`: per-node saturation (MASTERPLAN §10.2).
/// Bytes per second, at the scale it happens to be. Rounded hard on
/// purpose: this is a column somebody glances at to see whether a node is
/// moving anything, and `412M` answers that where `412.37 MB/s` makes them
/// read it.
fn rate(bps: u64) -> String {
    match bps {
        0 => "-".to_string(),
        b if b < 1024 => format!("{b}B"),
        b if b < 1024 * 1024 => format!("{}K", b / 1024),
        b if b < 1024 * 1024 * 1024 => format!("{}M", b / (1024 * 1024)),
        b => format!("{:.1}G", b as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

/// The `top` table as a string, with a sparkline of each node's recent CPU
/// beside it when the caller has been keeping one (`--follow`).
pub fn render_top(rows: &[TopRow], spark: Option<&crate::spark::History>) -> String {
    if rows.is_empty() {
        return "no nodes\n".to_string();
    }
    // A permanently empty column trains people to stop reading the row, so
    // a mesh that never runs a gang never sees this one (C4 P1).
    let show_leases = rows.iter().any(|r| r.leases > 0);
    let leased_header = if show_leases { "  LEASED" } else { "" };
    // And once more for accelerators (C5 P5): a mesh with no GPUs never
    // sees these two columns.
    let show_devices = rows.iter().any(|r| r.devices > 0);
    let _ = leased_header;
    let mut header = vec!["ST", "NODE", "NAME", "SLOTS", "RUNNING", "QUEUED"];
    if show_leases {
        header.push("LEASED");
    }
    // CPU first, because it is the column that answers the question
    // everyone opens this table to ask. `LOAD/c` stays beside it: they
    // disagree in a way that is informative — a box at 100% CPU with a
    // load of 0.2 per core is *busy*, and a box at 20% CPU with a load of
    // 4 per core is *thrashing on something else*.
    header.push("CPU");
    // The sparkline sits beside the number it explains, and only when
    // there is history to draw: a one-shot `top` has one reading, and one
    // reading is a number, not a line.
    let spark_col = spark.map(|_| header.len());
    if spark.is_some() {
        header.push("CPU HISTORY");
    }
    header.extend(["MEM", "DISK", "NET", "LOAD/c", "FREE"]);
    if show_devices {
        header.extend(["GPU", "GPU%"]);
    }
    header.push("SATURATION");
    // Every column after NAME is a number, except the sparkline.
    let right: Vec<usize> = (3..header.len() - 1)
        .filter(|i| Some(*i) != spark_col)
        .collect();
    let mut t = table(&header, &right);
    for r in rows {
        let name = if r.is_self {
            format!("{} (self)", r.name)
        } else {
            r.name.clone()
        };
        let saturation = if r.no_execute {
            "no-execute".to_string()
        } else if r.slots == 0 {
            "-".to_string()
        } else {
            let pct = (f64::from(r.running) / f64::from(r.slots) * 100.0).round();
            format!("{pct:.0}%")
        };
        // Load per core, not raw load: 8 on a 32-core box is idle. Both
        // columns read "-" when pressure is unknown, which is a fact about
        // this node's information, not about that node's health.
        let pct = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |p| format!("{p:.0}%"));
        let cpu = pct(r.system.as_ref().and_then(|s| s.cpu_pct));
        let mem = pct(r
            .system
            .as_ref()
            .and_then(mesh_proto::SystemSnapshot::mem_used_pct));
        let disk = pct(r
            .system
            .as_ref()
            .and_then(mesh_proto::SystemSnapshot::disk_used_pct));
        // Throughput, not a percentage: there is no link speed to divide
        // by that this machine can honestly report (see `net_rx_bps`).
        let net = r
            .system
            .as_ref()
            .and_then(mesh_proto::SystemSnapshot::net_total_bps)
            .map_or_else(|| "-".to_string(), rate);
        let load = r
            .pressure
            .as_ref()
            .and_then(|p| p.load_per_core(if r.cores > 0 { r.cores } else { r.slots }))
            .map_or_else(|| "-".to_string(), |l| format!("{l:.2}"));
        let free = r
            .pressure
            .as_ref()
            .and_then(|p| p.mem_available_mb)
            .map_or_else(|| "-".to_string(), gib);
        // Reservations, and the cores behind them: this is the column that
        // explains a node with free slots refusing work.
        let leased = format!("{}/{}c", r.leases, r.leased_cpus);
        // Devices held over devices advertised, then how busy the idlest
        // one is. The held figure is exact for this node and unknowable for
        // any other (a hold is node-local, §8.2's rule inherited), so a
        // remote row shows `?/4` rather than a number nobody can stand
        // behind.
        let held = match r.devices_busy {
            _ if r.devices == 0 => "-".to_string(),
            Some(busy) => format!("{busy}/{}", r.devices),
            None => format!("?/{}", r.devices),
        };
        let busy = r
            .device_busy
            .map_or_else(|| "-".to_string(), |b| format!("{:.0}%", b * 100.0));
        let mut row = vec![
            liveness_glyph(r.liveness).to_string(),
            r.node_id.short(),
            name,
            r.slots.to_string(),
            r.running.to_string(),
            r.queued.to_string(),
        ];
        if show_leases {
            row.push(leased);
        }
        row.push(cpu);
        if let Some(h) = spark {
            row.push(h.line(&r.node_id.to_display_string()));
        }
        row.extend([mem, disk, net, load, free]);
        if show_devices {
            row.extend([held, busy]);
        }
        row.push(saturation);
        t.add_row(row);
    }
    format!("{t}\n")
}

/// `top --devices`: one row per card, its utilisation and memory as bars,
/// and — with history — a sparkline of utilisation. The table a GPU box's
/// owner leaves open in a tmux pane.
pub fn render_devices_live(
    rows: &[mesh_proto::DeviceRow],
    spark: Option<&crate::spark::History>,
) -> String {
    if rows.is_empty() {
        return "no accelerators in this mesh\n  a node advertises what `nvidia-smi` or \
                `rocm-smi` reports; `meshnode doctor` says whether either is installed\n"
            .to_string();
    }
    let mut header = vec!["NODE", "IDX", "MODEL", "UTIL"];
    if spark.is_some() {
        header.push("UTIL HISTORY");
    }
    header.extend(["MEMORY", "TEMP", "HELD BY", "HEALTH"]);
    let mut t = table(&header, &[1, 6]);
    for r in rows {
        let mem = match (r.mem_free_mb, r.mem_mb) {
            (Some(free), total) if total > 0 => {
                let used = total.saturating_sub(free);
                format!(
                    "{} {}/{}",
                    crate::spark::bar(Some(used as f64 / total as f64 * 100.0), 8)
                        .split(' ')
                        .next()
                        .unwrap_or(""),
                    gib(used),
                    gib(total)
                )
            }
            (None, total) => format!("{:<8} ?/{}", "", gib(total)),
            _ => "-".to_string(),
        };
        let temp = r
            .temp_c
            .map_or_else(|| "-".to_string(), |t| format!("{t:.0}C"));
        let health = match (&r.health_reason, r.throttled) {
            (Some(reason), _) => format!("{} ({reason})", r.health),
            (None, Some(true)) => format!("{} (throttled)", r.health),
            (None, _) => r.health.clone(),
        };
        let mut row = vec![
            r.node_name.clone(),
            r.index.to_string(),
            r.model.clone(),
            crate::spark::bar(r.util_pct, 10),
        ];
        if let Some(h) = spark {
            row.push(h.line(&format!("{}:{}", r.node_name, r.index)));
        }
        row.extend([
            mem,
            temp,
            r.held_by.clone().unwrap_or_else(|| "free".into()),
            health,
        ]);
        t.add_row(row);
    }
    format!("{t}\n")
}

/// `results diff`: the rows that changed, worst first, and a tally of the
/// ones that did not.
pub fn print_run_diff(
    before: &str,
    after: &str,
    rows: &[crate::diff::Row],
    summary: &crate::diff::Summary,
    all: bool,
) {
    use crate::diff::Change;
    println!("{}   {before}", term::label("before"));
    println!("{}    {after}", term::label("after"));
    let mut parts = Vec::new();
    for (n, what) in [
        (summary.now_failing, "now failing"),
        (summary.changed, "changed"),
        (summary.still_failing, "still failing"),
        (summary.now_passing, "now passing"),
        (summary.only_after, "only after"),
        (summary.only_before, "only before"),
        (summary.same, "same"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    println!("{}  {}", term::label("outcome"), parts.join(", "));
    if summary.uncompared > 0 {
        println!(
            "         {} of the \"same\" rows compared exit codes only: no output was captured \
             on one side (launch a fan-out with --json or --outputs to compare output)",
            summary.uncompared
        );
    }
    let shown: Vec<&crate::diff::Row> = rows
        .iter()
        .filter(|r| all || r.change != Change::Same)
        .collect();
    if shown.is_empty() {
        println!();
        println!("nothing changed");
        return;
    }
    println!();
    let mut t = table(&["", "KEY", "BEFORE", "AFTER", "WHERE"], &[]);
    for r in shown {
        let side = |s: &Option<crate::diff::Side>| -> String {
            match s {
                None => "—".to_string(),
                Some(s) => match s.exit_code {
                    Some(0) => match &s.fingerprint {
                        Some(f) => format!("ok {}", &f[..8.min(f.len())]),
                        None => "ok".to_string(),
                    },
                    Some(c) => format!("exit {c}"),
                    None => "no answer".to_string(),
                },
            }
        };
        let where_ = match (&r.before, &r.after) {
            (Some(a), Some(b)) if a.node == b.node => a.node.clone(),
            (Some(a), Some(b)) => format!("{} → {}", a.node, b.node),
            (Some(a), None) => a.node.clone(),
            (None, Some(b)) => b.node.clone(),
            (None, None) => String::new(),
        };
        let mark = match r.change {
            Change::NowFailing => "✗",
            Change::Changed => "≠",
            Change::StillFailing => "✗",
            Change::NowPassing => "✓",
            Change::OnlyAfter => "+",
            Change::OnlyBefore => "−",
            Change::Same => "=",
        };
        t.add_row(vec![
            mark.to_string(),
            format!("{}  ({})", r.key, r.change.label()),
            side(&r.before),
            side(&r.after),
            where_,
        ]);
    }
    println!("{t}");
}

/// `nodes diff`: the keys where two nodes disagree, likely culprits first.
pub fn print_nodes_diff(
    left: &str,
    right: &str,
    rows: &[crate::diff::FactRow],
    same: usize,
    agree: &[(String, String)],
) {
    if rows.is_empty() {
        println!("{left} and {right} report the same {same} fact(s)");
    } else {
        println!(
            "{} fact(s) differ between {left} and {right}; {same} agree",
            rows.len()
        );
        println!();
    }
    let mut t = table(&["KEY", left, right], &[]);
    let cell = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
    let mut culprits_done = false;
    for r in rows {
        // A blank row between the likely culprits and the long tail, so
        // the eye stops where it should.
        if !culprits_done
            && !crate::diff::is_likely_culprit(&r.key)
            && rows
                .first()
                .is_some_and(|f| crate::diff::is_likely_culprit(&f.key))
        {
            t.add_row(vec![String::new(), String::new(), String::new()]);
            culprits_done = true;
        }
        t.add_row(vec![r.key.clone(), cell(&r.left), cell(&r.right)]);
    }
    if !agree.is_empty() {
        t.add_row(vec![String::new(), String::new(), String::new()]);
        for (k, v) in agree {
            t.add_row(vec![k.clone(), v.clone(), v.clone()]);
        }
    }
    if !rows.is_empty() || !agree.is_empty() {
        println!("{t}");
    }
}

/// `doctor --fix`: what was done about each finding.
pub fn print_doctor_fixes(fixes: &[mesh_core::doctor::FixOutcome]) {
    if fixes.is_empty() {
        println!("nothing to fix — every check was ok");
        return;
    }
    let applied = fixes.iter().filter(|f| f.applied).count();
    println!("{applied} of {} finding(s) fixed", fixes.len());
    for f in fixes {
        println!(
            // Wide enough for the longest check name, which is
            // `runtime container` since T4. A name that overflows its
            // column pushes every detail on that row out of alignment, and
            // a `doctor` that does not line up is one people skim.
            "{}  {:<18}  {}",
            if f.applied { "✔   " } else { "·   " },
            f.name,
            f.detail
        );
    }
}

/// `meshnode nodes --devices` (C5 P1).
///
/// One row per accelerator in the mesh, answered from replicated state — so
/// this works from any node, about every node, with no discovery of its own
/// and nobody asked. That is the property the inventory entity exists for.
pub fn print_devices(rows: &[mesh_proto::DeviceRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no accelerators in this mesh");
        println!(
            "{}",
            term::out().prose(
                "  a node advertises what `nvidia-smi` or `rocm-smi` reports; \
             `meshnode doctor` says whether either is installed"
            )
        );
        return Ok(());
    }
    let live = rows.iter().any(|r| r.util_pct.is_some());
    let held = rows.iter().any(|r| r.held_by.is_some());
    println!(
        "{}",
        term::out().dim(format!(
            "{:<14} {:>3} {:<6} {:<28} {:>8} {:>5}{}{}  HEALTH",
            "NODE",
            "IDX",
            "KIND",
            "MODEL",
            "MEM",
            "NUMA",
            if live { "   UTIL   FREE  TEMP" } else { "" },
            if held { "  HELD BY" } else { "" }
        ))
    );
    for r in rows {
        let telemetry = if live {
            format!(
                " {:>6} {:>6} {:>5}",
                r.util_pct
                    .map_or_else(|| "-".to_string(), |u| format!("{u:.0}%")),
                r.mem_free_mb.map_or_else(|| "-".to_string(), gib),
                r.temp_c
                    .map_or_else(|| "-".to_string(), |t| format!("{t:.0}C")),
            )
        } else {
            String::new()
        };
        let holder = if held {
            format!("  {}", r.held_by.as_deref().unwrap_or("-"))
        } else {
            String::new()
        };
        // A throttled device is not unhealthy — it is the reason the
        // identical job is slower here than on the box next door, which is
        // exactly the question this column exists to answer.
        let health = match (&r.health_reason, r.throttled) {
            (Some(reason), _) => format!("{} ({reason})", r.health),
            (None, Some(true)) => format!("{} (throttled)", r.health),
            (None, _) => r.health.clone(),
        };
        println!(
            "{:<14} {:>3} {:<6} {:<28} {:>8} {:>5}{telemetry}{holder}  {health}",
            truncate(&r.node_name, 14),
            r.index,
            r.kind,
            truncate(&r.model, 28),
            if r.mem_mb > 0 {
                gib(r.mem_mb)
            } else {
                "-".to_string()
            },
            r.numa_node
                .map_or_else(|| "-".to_string(), |n| n.to_string()),
        );
    }
    Ok(())
}

/// `meshnode sources ls` (MASTERPLAN §8.1).
pub fn print_sources(rows: &[SourceRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no git sources");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<12} {:<30} {:<10} {:<9} {:<12} {:<10} {:<10} BUMPS",
            "NAME", "URL", "BRANCH", "INTERVAL", "COMMIT", "SYNCED", "WATCHER"
        ))
    );
    for r in rows {
        let commit = r
            .last_commit
            .as_deref()
            .map(|c| c[..c.len().min(10)].to_string())
            .unwrap_or_else(|| "-".into());
        let synced = r
            .last_synced_at_micros
            .map(|t| micros_ago(now, t))
            .unwrap_or_else(|| "never".into());
        let bumps = if r.bump.is_empty() {
            "-".into()
        } else {
            r.bump.join(",")
        };
        println!(
            "{} {:<30} {:<10} {:<9} {:<12} {} {:<10} {}",
            term::out().bold(format!("{:<12}", truncate(&r.name, 12))),
            truncate(&r.url, 30),
            truncate(&r.branch, 10),
            format!("{}s", r.interval_secs),
            commit,
            term::out().dim(format!("{:<10}", synced)),
            truncate(r.watcher.as_deref().unwrap_or("-"), 10),
            bumps
        );
    }
    Ok(())
}

pub fn print_identity(i: &IdentityInfo, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(i)?);
        return Ok(());
    }
    println!("{}   {}", term::label("node id"), i.node_id);
    println!("node name {}", i.node_name);
    println!("{}   {}", term::label("user id"), i.user_id);
    println!(
        "{}      {}  ({})",
        term::label("mesh"),
        i.mesh_name.as_deref().unwrap_or("-"),
        i.mesh_id
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("{}  {}", term::label("data dir"), i.data_dir);
    Ok(())
}

pub fn print_ping(p: &PingResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(p)?);
        return Ok(());
    }
    println!(
        "reply from {} ({}) via {}  rtt={:.2}ms",
        p.peer_node_id.short(),
        p.peer_node_name,
        p.via,
        p.rtt_micros as f64 / 1000.0
    );
    match (p.same_mesh, p.peer_mesh_name.as_deref()) {
        (true, Some(name)) => println!("  same mesh: {name}"),
        // A node names its mesh only to a peer already in it (SEC-10).
        (false, Some("")) => println!("  different mesh (peer is not in your mesh)"),
        (false, Some(name)) => println!("  different mesh: {name} (peer is not in your mesh)"),
        _ => println!("  peer has no mesh"),
    }
    Ok(())
}

/// `meshnode nodes`.
pub fn print_nodes_with(rows: &[NodeRow], json: bool) -> Result<()> {
    let shown: Vec<&NodeRow> = rows.iter().collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&shown)?);
        return Ok(());
    }
    if shown.is_empty() {
        println!("no nodes");
        return Ok(());
    }
    let rows: Vec<NodeRow> = shown.into_iter().cloned().collect();
    let rows = &rows[..];
    let now = mesh_proto::now_micros();
    // Capacity gets a column of its own rather than being dumped as raw
    // `cap.*` tags (M9 §5.1); the column is dropped entirely on a mesh
    // where nobody advertises any.
    let show_capacity = rows.iter().any(|r| r.capacity.is_some());
    let mut header = vec!["ST", "NODE", "NAME", "ADDRESS"];
    if show_capacity {
        header.push("CAPACITY");
    }
    header.extend(["TAGS", "JOINED"]);
    let mut t = table(&header, &[]);
    for r in rows {
        let glyph = if r.revoked {
            "⊘"
        } else {
            liveness_glyph(r.liveness)
        };
        let name = if r.is_self {
            format!("{} (self)", r.name)
        } else {
            r.name.clone()
        };
        let addr = r.addrs.first().cloned().unwrap_or_else(|| "-".into());
        let mut shown_tags: Vec<String> = r.tags.clone();
        // The owner's veto reads as a tag because that is where an operator
        // looks for "why is this node not being used", even though it is
        // not one — it is a decision only an admin can author, and the node
        // cannot take it off by re-announcing.
        // Quarantine first, and it reads louder than the veto, because it
        // is a different statement: `no-contribute` is "we are not using
        // this machine", quarantine is "nobody go near it" (E2.4).
        if r.quarantined.is_some() {
            shown_tags.insert(0, "QUARANTINED".to_string());
        } else if !r.contributes {
            shown_tags.insert(0, "no-contribute".to_string());
        }
        let tags = if shown_tags.is_empty() {
            "-".into()
        } else {
            shown_tags.join(",")
        };
        // A pre-1.7 peer is reachable and healthy; what it is not is
        // fully caught up, and only on entity kinds it has no code for.
        // Say that on the row rather than letting it read as alive-and-fine
        // (F0 §3.3).
        let stale = if r.stale.is_some() {
            "  stale (pre-1.7)".to_string()
        } else if let Some(sy) = r.sync.filter(|s| s.behind > 0) {
            // Only when it is actually behind. A caught-up peer printing
            // "100%" on every row would be noise on the line that matters
            // least, and would make the rows that *do* lag harder to spot.
            format!(
                "  syncing {} op{} behind",
                sy.behind,
                if sy.behind == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let mut row = vec![glyph.to_string(), r.node_id.short(), name, addr];
        if show_capacity {
            row.push(capacity_summary(r.capacity.as_ref()));
        }
        row.push(tags);
        row.push(format!("{}{stale}", micros_ago(now, r.joined_at_micros)));
        t.add_row(row);
    }
    println!("{t}");
    let held: Vec<&NodeRow> = rows.iter().filter(|r| r.quarantined.is_some()).collect();
    if !held.is_empty() {
        println!();
        for r in &held {
            println!(
                "{} is QUARANTINED — {}",
                r.name,
                r.quarantined.as_deref().unwrap_or("no reason given")
            );
        }
        println!("It takes no work and nobody can open a terminal on it.");
        println!(
            "{}",
            term::out().prose("`meshnode nodes quarantine <node> --lift` reverses it.")
        );
    }
    let refused: Vec<&NodeRow> = rows
        .iter()
        .filter(|r| !r.contributes && r.quarantined.is_none())
        .collect();
    if !refused.is_empty() {
        println!();
        for r in &refused {
            println!(
                "{} is not being given work by decision of an admin{}.",
                r.name,
                if r.contribute_reason.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", r.contribute_reason)
                }
            );
        }
        println!(
            "{}",
            term::out().prose("`meshnode nodes contribute <node> --on` reverses it.")
        );
    }
    let stale: Vec<&NodeRow> = rows.iter().filter(|r| r.stale.is_some()).collect();
    if !stale.is_empty() {
        let ops: u64 = stale
            .iter()
            .filter_map(|r| r.stale)
            .map(|s| s.ops_withheld)
            .sum();
        println!();
        println!(
            "{} node{} predate the forward-compatible op-log and cannot receive {ops} op{}.",
            stale.len(),
            if stale.len() == 1 { "" } else { "s" },
            if ops == 1 { "" } else { "s" },
        );
        println!(
            "{}",
            term::out().prose("Upgrade them to clear it; `meshnode doctor` has the detail.")
        );
    }
    // Sync lag is normal and self-clearing — a mesh mid-gossip always has
    // some — so it gets a footnote rather than a warning, and only when
    // somebody is actually behind.
    let lagging: Vec<&NodeRow> = rows
        .iter()
        .filter(|r| r.stale.is_none() && r.sync.is_some_and(|s| s.behind > 0))
        .collect();
    if !lagging.is_empty() {
        let ops: u64 = lagging
            .iter()
            .filter_map(|r| r.sync)
            .map(|s| s.behind)
            .sum();
        println!();
        println!(
            "{} node{} {} catching up on {ops} op{} as of their last gossip round.",
            lagging.len(),
            if lagging.len() == 1 { "" } else { "s" },
            if lagging.len() == 1 { "is" } else { "are" },
            if ops == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

/// One-line capacity: `32c/125G/875G x86_64`. Unknown fields are elided
/// rather than printed as zero — a node that could not probe its disk says
/// nothing about disk, it does not claim to have none.
fn capacity_summary(cap: Option<&mesh_proto::Capacity>) -> String {
    let Some(cap) = cap else {
        return "-".into();
    };
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if cap.cores > 0 {
        parts.push(format!("{}c", cap.cores));
    }
    if cap.mem_mb > 0 {
        parts.push(gib(cap.mem_mb));
    }
    if cap.disk_mb > 0 {
        parts.push(gib(cap.disk_mb));
    }
    let mut out = parts.join("/");
    if !cap.arch.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&cap.arch);
    }
    out
}

/// MB as a compact `NNNG`/`N.NT`, the scale operators actually read.
fn gib(mb: u64) -> String {
    let gb = mb as f64 / 1024.0;
    if gb >= 1024.0 {
        format!("{:.1}T", gb / 1024.0)
    } else if gb >= 10.0 {
        format!("{gb:.0}G")
    } else {
        format!("{gb:.1}G")
    }
}

/// `meshnode totals` — the aggregate, with its denominators.
///
/// Every hardware figure is printed as "X across N of M nodes" whenever N
/// and M differ. That is not pedantry: a mesh where four nodes have not
/// advertised their memory yet has a real answer and a flattering one, and
/// printing the flattering one is how a headline number stops being
/// believed the first time somebody checks it against the rack.
pub fn print_totals(t: &Totals, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(t)?);
        return Ok(());
    }

    // The spec sheet first, then the breakdown that justifies it. The
    // strip is the sentence that gets pasted into a chat window and drawn
    // into the shared snapshot (L2 P2); leading with it means the line
    // somebody quotes and the table they check it against were produced by
    // one summation, phrased once.
    println!("{}", t.strip());
    if let Some(coverage) = t.coverage() {
        println!("{coverage}");
    }
    println!();

    // "36 of 40" only when they differ — a qualifier on every line trains
    // people to stop reading qualifiers.
    let of = |from: u32| -> String {
        if from == t.nodes_live || t.nodes_live == 0 {
            String::new()
        } else {
            format!("  ({from} of {} nodes)", t.nodes_live)
        }
    };

    let mut nodes = format!("{} live", t.nodes_live);
    if t.nodes_draining > 0 {
        nodes.push_str(&format!(" · {} draining", t.nodes_draining));
    }
    if t.nodes_known > t.nodes_live {
        nodes.push_str(&format!(" · {} unreachable", t.nodes_known - t.nodes_live));
    }
    println!("{} {nodes}", term::label_w("NODES", 10));

    if t.cores_from > 0 {
        let idle = match t.load_per_core {
            Some(l) => format!(
                " · {:.0}% idle",
                ((1.0 - l).clamp(0.0, 1.0) * 100.0).round()
            ),
            None => String::new(),
        };
        println!(
            "{} {} logical{idle}{}",
            term::label_w("CORES", 10),
            t.cores,
            of(t.cores_from)
        );
    }

    // Slots, not cores, are the concurrency ceiling — a policy number that
    // is routinely different from the hardware one, and the number that
    // actually decides how much of this mesh a job can have.
    if t.slots > 0 {
        let pct = (f64::from(t.slots_busy) / f64::from(t.slots) * 100.0).round();
        println!(
            "{} {} total · {} busy ({pct:.0}%)",
            term::label_w("SLOTS", 10),
            t.slots,
            t.slots_busy
        );
    }

    if t.mem_from > 0 {
        let free = if t.mem_available_from > 0 {
            format!(" · {} available", gib(t.mem_available_mb))
        } else {
            String::new()
        };
        println!(
            "{} {} total{free}{}",
            term::label_w("MEMORY", 10),
            gib(t.mem_mb),
            of(t.mem_from)
        );
    }

    if t.disk_from > 0 {
        let free = if t.disk_free_from > 0 {
            format!(" · {} free", gib(t.disk_free_mb))
        } else {
            String::new()
        };
        println!(
            "{} {} total{free}{}",
            term::label_w("DISK", 10),
            gib(t.disk_mb),
            of(t.disk_from)
        );
    }

    // Both of these stay out of the way on a mesh that has none, the same
    // rule `print_top` follows for its GPU and lease columns.
    if t.devices > 0 {
        // The GPU half is called out because it is the one people budget
        // in: "4 accelerators" does not tell you whether a 40 GB model
        // fits anywhere, and the memory total does.
        let gpus = match (t.gpus, t.vram_mb) {
            (0, _) => String::new(),
            (n, 0) => format!(" · {n} GPU"),
            (n, mb) => format!(" · {n} GPU, {} VRAM", gib(mb)),
        };
        println!(
            "{} {} advertised{gpus} · {} held here",
            term::label_w("DEVICES", 10),
            t.devices,
            t.devices_busy_here
        );
    }

    if t.jobs_running > 0 || t.jobs_queued > 0 {
        println!(
            "{} {} running · {} queued",
            term::label_w("JOBS", 10),
            t.jobs_running,
            t.jobs_queued
        );
    }

    if !t.arches.is_empty() {
        let list = t
            .arches
            .iter()
            .map(|(a, n)| format!("{a} ×{n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        println!("{} {list}", term::label_w("ARCH", 10));
    }

    Ok(())
}

/// `meshnode invite ls` (SEC-17).
pub fn print_invites(rows: &[mesh_proto::InviteRow], all: bool, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", if all { "no invites" } else { "no open invites" });
        if !all {
            println!("\nSpent, expired and revoked ones: meshnode invite ls --all");
        }
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<24} {:<16} {:>9}  {:<18}  TAGS",
            "INVITE", "BY", "USED", "STATE"
        ))
    );
    let now = mesh_proto::now_micros();
    for r in rows {
        let state = if r.revoked {
            "revoked".to_string()
        } else if r.used >= r.uses {
            "used up".to_string()
        } else if r.expires_at_micros <= now {
            format!(
                "expired {} ago",
                short_duration(now.saturating_sub(r.expires_at_micros))
            )
        } else {
            format!(
                "{} left",
                short_duration(r.expires_at_micros.saturating_sub(now))
            )
        };
        let tags: Vec<String> = r
            .tags
            .iter()
            .map(|t| mesh_proto::text::clean(t, mesh_proto::text::MAX_NAME_CHARS))
            .collect();
        println!(
            "{} {:<16} {:>9}  {}  {}",
            term::out().dim(format!("{:<24}", r.invite_id.to_display_string())),
            mesh_proto::text::clean(&r.created_by_username, 16),
            format!("{}/{}", r.used, r.uses),
            term::out().state(format!("{:<18}", state)),
            tags.join(",")
        );
    }
    Ok(())
}

/// A coarse duration, for a column where minutes are the useful unit.
fn short_duration(micros: u64) -> String {
    let secs = micros / 1_000_000;
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

pub fn print_jobs(rows: &[JobRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no jobs");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    let mut t = table(
        &[
            "JOB", "STATE", "RUNTIME", "EXECUTOR", "USER", "CODE", "AGE", "COMMAND",
        ],
        &[5],
    );
    for r in rows {
        t.add_row(vec![
            r.job_id.short(),
            r.state.as_str().to_string(),
            r.runtime.clone(),
            r.executor_name
                .clone()
                .unwrap_or_else(|| r.executor.short()),
            r.submitter_username
                .clone()
                .unwrap_or_else(|| r.submitter_user.short()),
            r.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            micros_ago(now, r.created_at_micros),
            r.cmd_preview.clone(),
        ]);
    }
    println!("{t}");
    Ok(())
}

/// [`print_jobs`] for a paged answer (**H-1**), with the footer a bounded
/// list needs: a table that shows 50 of 40 000 and does not say so is a
/// table that has quietly lied about what ran.
pub fn print_jobs_page(page: &mesh_proto::JobsPage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(page)?);
        return Ok(());
    }
    print_jobs(&page.rows, false)?;
    let shown = page.rows.len() as u64;
    if shown < page.total {
        println!(
            "\nshowing {shown} of {} matching — --limit/--offset for more",
            page.total
        );
    }
    let c = page.counts;
    if c.all > shown {
        println!(
            "{} jobs known · {} live · {} succeeded · {} failed",
            c.all,
            c.live(),
            c.succeeded,
            c.unsuccessful()
        );
    }
    Ok(())
}

pub fn print_job(r: &JobRow, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!("{}       {}", term::label("job"), r.job_id);
    println!(
        "{}     {}",
        term::label("state"),
        term::out().state(r.state.as_str())
    );
    println!(
        "{}  {} ({})",
        term::label("executor"),
        r.executor.short(),
        r.executor_name.as_deref().unwrap_or("-")
    );
    println!("{} {}", term::label("submitter"), r.submitter.short());
    println!(
        "{} {}",
        term::label("exit code"),
        r.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("{}   {}", term::label("command"), r.cmd_preview);
    // M31 A2: say where the output is, on the record itself, so the answer
    // is here rather than discovered by running `job logs` and getting
    // nothing.
    if !r.output.is_available() {
        println!();
        println!("{}", r.output.message());
    }
    Ok(())
}

pub fn print_bundles(rows: &[BundleRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no bundles");
        return Ok(());
    }
    let mut t = table(
        &["BUNDLE", "SIZE", "FILES", "CHUNKS", "HOLDERS", "LOCAL"],
        &[1, 2, 3, 4],
    );
    for r in rows {
        t.add_row(vec![
            r.bundle_id.short(),
            human_bytes(r.total_size),
            r.file_count.to_string(),
            r.chunk_count.to_string(),
            r.holders.to_string(),
            if r.local { "yes" } else { "no" }.to_string(),
        ]);
    }
    println!("{t}");
    Ok(())
}

/// Ephemeral objects held on this node (MASTERPLAN M9 §5.5). `RESIDENT`
/// is what is still in RAM; the rest has spilled to disk, which is the
/// residency model working rather than a problem.
pub fn print_objects(rows: &[ObjectRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no objects held here");
        return Ok(());
    }
    let mut t = table(
        &["OBJECT", "SIZE", "RESIDENT", "AGE", "JOB", "STATE"],
        &[1, 2, 3],
    );
    for r in rows {
        t.add_row(vec![
            r.id.short(),
            human_bytes(r.size),
            human_bytes(r.resident),
            format!("{}s", r.age_secs),
            r.job.short(),
            if r.awaiting_collection {
                "waiting for you".into()
            } else {
                String::new()
            },
        ]);
    }
    println!("{t}");
    // The difference between "this will be gone in fifteen minutes" and
    // "this is the answer to something you ran" is the whole of R3 P3, and
    // a table that did not say which is which would be the old behaviour
    // with extra rows.
    let waiting: Vec<&ObjectRow> = rows.iter().filter(|r| r.awaiting_collection).collect();
    if !waiting.is_empty() {
        let bytes: u64 = waiting.iter().map(|r| r.size).sum();
        println!(
            "\n{} object(s), {}, are finished results nobody has fetched. They are held \
             past the usual expiry until you do:\n  meshnode map --resume … --out DIR, or \
             Save results… in Director",
            waiting.len(),
            human_bytes(bytes)
        );
    }
    Ok(())
}

pub fn print_services(rows: &[ServiceRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no services");
        return Ok(());
    }
    let mut t = table(&["NAME", "RUNTIME", "VER", "CONTRACT", "ENTRYPOINT"], &[2]);
    for r in rows {
        t.add_row(vec![
            r.name.clone(),
            r.runtime.as_str().to_string(),
            r.version.to_string(),
            if r.has_contract { "yes" } else { "-" }.to_string(),
            r.entrypoint.clone(),
        ]);
    }
    println!("{t}");
    Ok(())
}

pub fn print_service(r: &ServiceRow, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!("{}     {}", term::label("service"), r.name);
    println!("{}     {}", term::label("runtime"), r.runtime.as_str());
    println!(
        "{}     {} (of {:?})",
        term::label("version"),
        r.version,
        r.versions
    );
    println!("{}      {}", term::label("bundle"), r.bundle_id.short());
    println!(
        "working_dir {}",
        if r.working_dir.is_empty() {
            "(root)"
        } else {
            &r.working_dir
        }
    );
    println!("{}  {}", term::label("entrypoint"), r.entrypoint);
    println!(
        "{}    {}",
        term::label("contract"),
        if r.has_contract { "yes" } else { "none" }
    );
    if !r.tags.is_empty() {
        println!("{}        {}", term::label("tags"), r.tags.join(","));
    }
    if !r.description.is_empty() {
        println!("description {}", r.description);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workflows (M10 §4)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Facts (M18 §4.9)
// ---------------------------------------------------------------------------

/// A fact document as a key/value table, flattened.
///
/// Flattened rather than pretty-printed JSON: the keys are what a selector
/// addresses, so showing `disks.0.fstype` is showing the thing somebody
/// would type — and a nested view would hide it behind indentation.
pub fn print_facts(rows: &[mesh_proto::FactRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no facts yet");
        println!(
            "{}",
            term::out().prose(&format!(
                "  the built-in `system` collector publishes within {}s of a node starting",
                mesh_proto::SYSTEM_INTERVAL_SECS
            ))
        );
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!(
            "{} · {}   observed {} by {}",
            row.subject_name,
            row.collector,
            micros_ago(now, row.collected_at_micros),
            row.author_name
        );
        let doc = mesh_core::facts::parse(&row.doc);
        let flat = mesh_core::facts::flatten(&doc);
        let width = flat.iter().map(|(k, _)| k.len()).max().unwrap_or(4).min(32);
        for (key, value) in &flat {
            // An array publishes both a joined form and its indices; the
            // joined form is the one a person reads.
            if key
                .rsplit_once('.')
                .is_some_and(|(_, tail)| tail.parse::<usize>().is_ok())
            {
                continue;
            }
            println!("  {key:<width$}  {}", truncate(value, 96));
        }
    }
    Ok(())
}

pub fn print_fact_matches(
    expression: &str,
    subjects: &[(String, String)],
    unknown: &[String],
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "expression": expression,
                "matched": subjects
                    .iter()
                    .map(|(name, value)| serde_json::json!({"subject": name, "value": value}))
                    .collect::<Vec<_>>(),
                "unknown": unknown,
            }))?
        );
        return Ok(());
    }
    if subjects.is_empty() {
        println!("nothing matches {expression}");
    } else {
        let width = subjects
            .iter()
            .map(|(n, _)| n.len())
            .max()
            .unwrap_or(7)
            .min(28);
        for (name, value) in subjects {
            println!("{name:<width$}  {}", truncate(value, 64));
        }
        println!();
        println!("{} subject(s) match {expression}", subjects.len());
    }
    // Not a footnote. "Nine of your six hundred boxes do not say what
    // kernel they run" is the answer somebody needs *before* patching, and
    // a query that omitted them would read as "all six hundred are fine".
    if !unknown.is_empty() {
        println!(
            "{} subject(s) have facts but no such key, so they match nothing: {}",
            unknown.len(),
            summarize_names(unknown)
        );
    }
    Ok(())
}

pub fn print_fact_diff(rows: &[mesh_proto::FactDiffRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing changed in that window");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    for row in rows {
        println!(
            "{}  {} · {}",
            micros_ago(now, row.at_micros),
            row.subject_name,
            row.collector
        );
        for change in &row.changes {
            match (&change.before, &change.after) {
                (Some(before), Some(after)) => println!(
                    "    {}  {} → {}",
                    change.key,
                    truncate(before, 40),
                    truncate(after, 40)
                ),
                (None, Some(after)) => {
                    println!("  + {}  {}", change.key, truncate(after, 60));
                }
                (Some(before), None) => {
                    println!("  - {}  {}", change.key, truncate(before, 60));
                }
                (None, None) => {}
            }
        }
    }
    Ok(())
}

pub fn print_collectors(rows: &[mesh_proto::FactCollectorRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<8} {:<14} {:<10} {:<8} {:<9} RUN",
            "COLLECTOR", "SUBJECT", "ON", "EVERY", "STATE", "SUBJECTS"
        ))
    );
    for r in rows {
        let state = if r.built_in {
            "built-in"
        } else if r.paused {
            "paused"
        } else if !r.flapping_keys.is_empty() {
            "FLAPPING"
        } else {
            "armed"
        };
        println!(
            "{} {:<8} {:<14} {:<10} {} {:<9} {}",
            term::out().bold(format!("{:<16}", truncate(&r.name, 16))),
            r.subject,
            truncate(&r.on, 14),
            humantime::format_duration(std::time::Duration::from_secs(r.interval_secs)).to_string(),
            term::out().state(format!("{:<8}", state)),
            r.subjects,
            truncate(&r.run, 40)
        );
    }
    // A flapping collector is a correctness problem, not a style one: it
    // would write an unbounded, uncompactable op-log. Naming the keys is
    // what turns "your collector is wrong" into "your collector emits
    // `uptime`".
    for r in rows.iter().filter(|r| !r.flapping_keys.is_empty()) {
        println!();
        println!(
            "! {} is publishing faster than once an hour. These keys changed when \
             nothing changed: {}",
            r.name,
            r.flapping_keys.join(", ")
        );
        println!("  A fact is a property of the machine, not a measurement of it.");
    }
    Ok(())
}

fn summarize_names(names: &[String]) -> String {
    const MAX: usize = 6;
    if names.len() <= MAX {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..MAX].join(", "),
        names.len() - MAX
    )
}

// ---------------------------------------------------------------------------
// Results (M19 §5.4)
// ---------------------------------------------------------------------------

/// One row as JSON: the shape `--format json` emits, and the shape a
/// script consumes. Outputs are an object rather than an array of pairs
/// because `jq '.[] | .outputs.stdout_json'` is what people will type.
pub fn result_json(r: &mesh_proto::ResultRow) -> serde_json::Value {
    let outputs: serde_json::Map<String, serde_json::Value> = r
        .outputs
        .iter()
        .map(|(k, v)| {
            // `stdout_json` is JSON that was re-encoded as a string on its
            // way through the record. Re-parsing it here is what lets
            // `jq .outputs.stdout_json.total` work instead of returning a
            // string that happens to look like JSON.
            let value = if k == mesh_core::outputs::OUT_STDOUT_JSON {
                serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.clone()))
            } else {
                serde_json::Value::String(v.clone())
            };
            (k.clone(), value)
        })
        .collect();
    let mut row = serde_json::json!({
        "node": r.node_name,
        "node_id": r.node.to_display_string(),
        "job": r.job_id.to_display_string(),
        "state": r.state.as_str(),
        "exit_code": r.exit_code,
        "outputs": outputs,
    });
    if let Some(passed) = r.expect_passed
        && let Some(obj) = row.as_object_mut()
    {
        obj.insert("expect_passed".into(), serde_json::Value::Bool(passed));
        obj.insert(
            "expect_detail".into(),
            serde_json::Value::String(r.expect_detail.clone()),
        );
    }
    row
}

pub fn print_result_rows(rows: &[mesh_proto::ResultRow], json: bool) -> Result<()> {
    if json {
        let arr: Vec<serde_json::Value> = rows.iter().map(result_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no rows");
        return Ok(());
    }
    // Which columns exist is decided by the rows, not by a fixed list: a
    // fan-out that published `sha` should show `sha`, and one that
    // published nothing but the implicit four should not show four empty
    // columns for the ones nobody printed.
    let mut keys: Vec<String> = Vec::new();
    for row in rows {
        for (k, v) in &row.outputs {
            if !v.trim().is_empty() && !keys.iter().any(|e| e == k) {
                keys.push(k.clone());
            }
        }
    }
    // Declared outputs first, then the implicit four in their own order —
    // what the author named is what they will look for first.
    keys.sort_by_key(|k| {
        mesh_core::outputs::IMPLICIT_OUTPUTS
            .iter()
            .position(|i| i == k)
            .map(|p| p + 1)
            .unwrap_or(0)
    });
    let asserted = rows.iter().any(|r| r.expect_passed.is_some());

    let mut header = format!("{:<16} {:<6}", "NODE", "CODE");
    if asserted {
        header.push_str(&format!(" {:<7}", "EXPECT"));
    }
    for k in &keys {
        header.push_str(&format!(" {:<28}", k.to_uppercase()));
    }
    println!("{}", header.trim_end());
    for r in rows {
        let mut line = format!(
            "{:<16} {:<6}",
            truncate(&r.node_name, 16),
            r.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        );
        if asserted {
            line.push_str(&format!(
                " {:<7}",
                match r.expect_passed {
                    Some(true) => "pass",
                    Some(false) => "FAIL",
                    None => "-",
                }
            ));
        }
        for k in &keys {
            let value = r.output(k).unwrap_or("");
            line.push_str(&format!(" {:<28}", truncate(&abbreviate_output(value), 28)));
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

pub fn print_result_set(set: &mesh_proto::ResultSet, json: bool) -> Result<()> {
    if json {
        let arr: Vec<serde_json::Value> = set.rows.iter().map(result_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "group": set.group.to_display_string(),
                "owner": set.owner_name,
                "command": set.preview,
                "succeeded": set.succeeded(),
                "failed": set.failed(),
                "missing": set.missing,
                "rows": arr,
            }))?
        );
        return Ok(());
    }
    println!("{}    {}", term::label("group"), set.group);
    println!("{}    {}", term::label("owner"), set.owner_name);
    println!("{}  {}", term::label("command"), set.preview);
    println!(
        "{}  {} succeeded, {} failed{}",
        term::label("outcome"),
        set.succeeded(),
        set.failed(),
        if set.missing > 0 {
            format!(", {} never dispatched", set.missing)
        } else {
            String::new()
        }
    );
    println!();
    print_result_rows(&set.rows, false)
}

// ---------------------------------------------------------------------------
// Deep links (L2 P5)
// ---------------------------------------------------------------------------
//
// A command that produced one viewable object says where to look at it, and
// the address is one somebody else's machine understands. A job id pasted
// into a chat window is a string a colleague has to know what to do with;
// `director://jobs/job-…` is a string their machine knows what to do with.
//
// Three rules hold this to something honest:
//
//   1. **One object, or nothing.** A table is not a link. `jobs ls` prints
//      no line; `jobs show <id>` does.
//   2. **stderr, never stdout.** The link is a courtesy from the tool, not
//      the command's output, and `meshnode run … > out.txt` must not find
//      one in the file. This is the rule L1 P5's `←` lines already follow.
//   3. **Only to a terminal.** In a pipe, a log, or CI it is noise nobody
//      can click, so it is not written at all — the same test the
//      greeting's dim line applies to its own styling.

/// The address of one viewable object.
///
/// `director://<kind>/<id>` — deliberately not `https://`, because there is
/// no server in this product and a link that implied one would be the first
/// thing the mesh ever said that was not true of it.
pub fn deep_link(kind: DeepLinkKind, id: &str) -> String {
    format!("director://{}/{id}", kind.as_str())
}

/// What a deep link can point at. Every variant has a Director screen that
/// opens on exactly that object; there is no variant for "roughly there".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepLinkKind {
    /// One job, by full id.
    Job,
    /// Every job of one fan-out, by fan id.
    Fanout,
    /// One node, by full id.
    Node,
    /// One registered script, by name.
    Script,
    /// One deployment, by name (Phase B, U12).
    Deployment,
}

impl DeepLinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DeepLinkKind::Job => "jobs",
            DeepLinkKind::Fanout => "fanouts",
            DeepLinkKind::Node => "nodes",
            DeepLinkKind::Script => "scripts",
            DeepLinkKind::Deployment => "deployments",
        }
    }
}

/// Print the link as the command's last line — on stderr, and only to a
/// terminal (see the rules above). Dimmed, because it is a courtesy and
/// not the answer, and clickable where the terminal takes OSC 8 links.
pub fn print_deep_link(kind: DeepLinkKind, id: &str) {
    let caps = crate::term::Caps::detect();
    if caps.err.tty {
        eprintln!("{}", caps.link_line(&deep_link(kind, id)));
    }
}

// ---------------------------------------------------------------------------
// Say what happened (L1 P5)
// ---------------------------------------------------------------------------
//
// One distinction runs through everything below, and it is the only one a
// person actually needs at the moment a job ends: **did your command fail,
// or did the mesh have a problem?** They are different problems with
// different next steps, and the product used to print `failed` for both.
//
// The evidence is already in the frame. A job that produced an exit code
// *ran*: the mesh delivered it, the executor started it, and the number is
// the program's own verdict. A job with no exit code never produced one,
// which means nobody ever got an answer back — an unreachable node, an
// executor that died, a job that was stopped. Everything here is a reading
// of that one fact.
//
// D-L1.4: this is a pass over existing output, not a second formatter.
// These functions are the only place the sentences live, and `main.rs`
// calls them.

/// Did the command answer at all, and what did it say?
///
/// `TimedOut` and a `Failed` with no exit code are both "the mesh has no
/// answer for you" — the first because it stopped waiting, the second
/// because the answer never arrived.
pub fn job_outcome(state: mesh_proto::JobState, exit_code: Option<i32>) -> String {
    use mesh_proto::JobState;
    match (state, exit_code) {
        (JobState::Succeeded, _) => "succeeded".to_string(),
        (JobState::Failed, Some(code)) => format!("your command exited {code}"),
        (JobState::Failed, None) => "no answer — the mesh never got a result back".to_string(),
        (JobState::TimedOut, _) => "timed out — the mesh stopped waiting".to_string(),
        (JobState::Cancelled, _) => "cancelled".to_string(),
        (JobState::Queued, _) => "still queued".to_string(),
        (JobState::Running, _) => "still running".to_string(),
    }
}

/// Was this ending the mesh's fault rather than the command's?
///
/// The tally a fan-out summary is built from. A cancelled job is neither —
/// somebody asked for it — so it counts as neither.
pub fn is_no_answer(state: mesh_proto::JobState, exit_code: Option<i32>) -> bool {
    use mesh_proto::JobState;
    matches!(state, JobState::TimedOut) || (state == JobState::Failed && exit_code.is_none())
}

/// How a piece of a status line should read. The words live here; what
/// each tone looks like on a given terminal is `term::Paint`'s business,
/// and on a terminal with no colour every tone is plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// The part of the line a reader's eye should land on.
    Strong,
    /// Detail: ids, timings, separators.
    Quiet,
    /// It worked.
    Good,
    /// Your command failed.
    Bad,
    /// The mesh does not know — no answer came back. Also anything that
    /// wants a look: pending, stale, suspect.
    Unknown,
    /// In progress, or something to type.
    Accent,
}

/// What a fan-out's frames added up to, by name wherever the frames said
/// which machine it was.
#[derive(Debug, Clone, Default)]
pub struct FanTally {
    pub succeeded: u32,
    /// Machines whose command exited non-zero.
    pub exited_nonzero: Vec<String>,
    /// Machines that sent back no answer.
    pub no_answer: Vec<String>,
    /// Targets the origin gave up on without a frame for any of them: no
    /// answer, but no name either.
    pub no_answer_unnamed: u32,
    pub cancelled: u32,
    pub skipped: u32,
    pub took: std::time::Duration,
}

impl FanTally {
    fn total(&self) -> u32 {
        self.succeeded
            + self.exited_nonzero.len() as u32
            + self.no_answer.len() as u32
            + self.no_answer_unnamed
            + self.cancelled
            + self.skipped
    }
}

/// "NIM", "NIM and APUS", "IVY, NIM and APUS", "IVY, NIM and 3 more".
pub fn some_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] if names.len() <= 3 => format!("{} and {last}", init.join(", ")),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// A duration as a fan-out reports it — `duration_micros`, so a run reads
/// the same here as it does in `history`.
pub fn took(d: std::time::Duration) -> String {
    duration_micros(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// How a fan-out ended: one line, and — when something went wrong — one
/// sentence naming *which kind* of wrong it was, and where.
///
/// This is L1 P5's exit demo: a fan-out with two unreachable nodes has to
/// say so, and a reader who has never used meshware has to be able to say
/// what went wrong. `2 succeeded, 2 failed` failed that test twice over —
/// it did not say what failed, and it blamed the script for the network.
/// The sentences name the machines rather than count them, because the
/// next thing a reader does is go and look at one.
///
/// Returned as toned spans rather than printed, so the copy — which this
/// document says is the deliverable — has a test, and the renderer decides
/// what a tone looks like.
pub fn fanout_outcome_lines(t: &FanTally) -> Vec<Vec<(Tone, String)>> {
    let total = t.total();
    let machines = |n: u32| format!("{n} machine{}", if n == 1 { "" } else { "s" });
    let span = |tone: Tone, text: &str| (tone, text.to_string());
    let took = format!(" · {}", took(t.took));
    let no_answer = t.no_answer.len() as u32 + t.no_answer_unnamed;

    if t.exited_nonzero.is_empty() && no_answer == 0 && t.skipped == 0 && t.cancelled == 0 {
        let all = match total {
            1 => "1 machine succeeded".to_string(),
            2 => "Both machines succeeded".to_string(),
            n => format!("All {n} machines succeeded"),
        };
        return vec![vec![
            span(Tone::Good, "✓ "),
            span(Tone::Strong, &all),
            span(Tone::Quiet, &took),
        ]];
    }

    let lead = if !t.exited_nonzero.is_empty() {
        span(Tone::Bad, "✗ ")
    } else if no_answer > 0 {
        span(Tone::Unknown, "? ")
    } else {
        span(Tone::Quiet, "· ")
    };
    let mut head = vec![
        lead,
        span(
            Tone::Strong,
            &format!("{} of {total} succeeded", t.succeeded),
        ),
    ];
    let mut part = |tone: Tone, text: String| {
        head.push(span(Tone::Quiet, " · "));
        head.push((tone, text));
    };
    if !t.exited_nonzero.is_empty() {
        part(
            Tone::Bad,
            format!("{} exited non-zero", t.exited_nonzero.len()),
        );
    }
    if no_answer > 0 {
        part(Tone::Unknown, format!("{no_answer} never answered"));
    }
    if t.cancelled > 0 {
        part(Tone::Quiet, format!("{} cancelled", t.cancelled));
    }
    if t.skipped > 0 {
        part(Tone::Quiet, format!("{} never started", t.skipped));
    }
    head.push(span(Tone::Quiet, &took));
    let mut out = vec![head];

    // …and the sentence that says whose problem each half is. Written out
    // rather than implied: "failed" is the word that has been doing both
    // jobs, and the whole point of this line is to stop it.
    if !t.exited_nonzero.is_empty() {
        out.push(vec![
            span(Tone::Plain, "  Your command failed on "),
            span(Tone::Strong, &some_names(&t.exited_nonzero)),
            span(
                Tone::Plain,
                " — the mesh delivered it and got its answer back.",
            ),
        ]);
    }
    if no_answer > 0 {
        // Names only when every one of them is known; "APUS and 2
        // machines" would be a sentence nobody could act on.
        let who = if t.no_answer_unnamed == 0 {
            some_names(&t.no_answer)
        } else {
            machines(no_answer)
        };
        out.push(vec![
            span(Tone::Plain, "  The mesh could not get an answer from "),
            span(Tone::Strong, &who),
            span(Tone::Plain, " — unreachable, or the job was lost."),
        ]);
        out.push(vec![span(
            Tone::Plain,
            "  Whether your command ran there is unknown.",
        )]);
    }
    if t.skipped > 0 {
        out.push(vec![span(
            Tone::Plain,
            &format!(
                "  {} never started, because --halt-on-fail stopped the fan-out.",
                machines(t.skipped)
            ),
        )]);
    }
    out
}

/// The terminal line of an `--expect` run. A failure names the rows, and
/// says what the evaluator saw — a count is not a diagnosis.
pub fn print_expectation(expression: &str, passed: u32, failed: &[String]) {
    if failed.is_empty() {
        eprintln!(
            "{}",
            crate::term::err().status(&format!("✓ {expression} — {passed} row(s) passed"))
        );
        return;
    }
    eprintln!(
        "{}",
        crate::term::err().status(&format!(
            "✗ {expression} — {} of {} row(s) did not comply:",
            failed.len(),
            passed as usize + failed.len()
        ))
    );
    for line in failed {
        eprintln!("    {line}");
    }
}

// ---------------------------------------------------------------------------
// Runs (M10 P2)
// ---------------------------------------------------------------------------

/// Shorten a step output for display. A hash is the point; a 40 KB JSON
/// blob is not, and an object ref should read as one.
pub fn abbreviate_output(value: &str) -> String {
    if let Some((id, size)) = mesh_core::outputs::parse_object_ref(value) {
        return format!("<object {} · {}>", id.short(), human_bytes(size));
    }
    const MAX: usize = 72;
    let one_line = value.replace('\n', " ");
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let cut: String = one_line.chars().take(MAX - 1).collect();
        format!("{cut}…")
    }
}

/// `meshnode whoami` (M23 §4).
///
/// The "signed but denied" state is, from the outside, indistinguishable
/// from the product being broken — so it is the one state that has to
/// explain itself best. This function answers the question people actually
/// have, which is not *who am I* but *what do I do about it*.
/// `whoami` when the node would not answer — which is the state the person
/// most needs it in.
///
/// Half of the answer is local and always available: this is your key, this
/// is the file it lives in. The other half (roles, grants, whether a join
/// request is pending) belongs to the mesh, and the mesh is exactly what is
/// not talking to us — so say which half is missing and why, rather than
/// printing nothing and an error code.
pub fn print_whoami_offline(
    user: &mesh_proto::UserId,
    key_path: &std::path::Path,
    why: &str,
    json: bool,
) -> Result<()> {
    let key = user.to_display_string();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "userId": key,
                "identityPath": key_path.display().to_string(),
                "reachedNode": false,
                "error": why,
            }))?
        );
        return Ok(());
    }
    println!("This machine could not ask the mesh who you are.");
    println!();
    println!("  {} {}", term::label_w("Your key", 10), key);
    println!("  {} {}", term::label_w("", 10), tilde(key_path));
    println!();
    println!(
        "{}",
        term::out().prose("Send that key to an administrator of the mesh, or run `meshnode enrol`")
    );
    println!("from a machine that can reach it.");
    println!();
    println!("{why}");
    Ok(())
}

pub fn print_whoami(info: &WhoAmIInfo, key_path: &std::path::Path, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(info)?);
        return Ok(());
    }

    // Everyone here is identified by a key on their own machine. Say it
    // once, at the top, in the state where it is not yet obvious.
    match info.enrolment {
        mesh_proto::EnrolmentState::Enrolled => {}
        mesh_proto::EnrolmentState::Pending => {
            println!("You have asked to join this mesh. Nobody has decided yet.");
            println!();
        }
        mesh_proto::EnrolmentState::Declined => {
            println!("Your request to join this mesh was declined.");
            println!();
        }
        mesh_proto::EnrolmentState::Expired => {
            println!("Your request to join this mesh expired without a decision.");
            println!();
        }
        mesh_proto::EnrolmentState::None => {
            println!("You are not enrolled on this mesh.");
            println!();
        }
    }

    let key = info.user_id.to_display_string();
    println!("  {} {}", term::label_w("Your key", 10), key);
    println!("  {} {}", term::label_w("", 10), tilde(key_path));
    println!("  {} {}", term::label_w("This node", 10), info.node_name);
    if let Some(name) = &info.username {
        println!("  {} {}", term::label_w("Username", 10), name);
    }
    if let Some(code) = &info.enrolment_code {
        println!("  {} {}", term::label_w("Code", 10), code);
    }
    if info.registered {
        println!(
            "  {} {}",
            term::label_w("Roles", 10),
            if info.roles.is_empty() {
                "none — you are a user, but nothing is granted yet".to_string()
            } else {
                info.roles.join(", ")
            }
        );
    }
    if info.is_founder {
        println!(
            "  {} founder — implicit admin, from genesis",
            term::label_w("", 10)
        );
    }

    for g in &info.grants {
        println!(
            "  {} {}",
            term::label_w("Grant", 10),
            describe_grant(&g.role, &g.nodes, &g.services, g.expires_at_micros)
        );
        if g.revoked || g.expired {
            println!(
                "  {} {}",
                term::label_w("", 10),
                if g.revoked { "revoked" } else { "expired" }
            );
        }
    }

    // What to do about it. Every branch ends in a command a person can
    // type — the previous version of this screen ended in a command an
    // *admin* had to type, somewhere else, which is exactly the gap M23
    // is about.
    match info.enrolment {
        mesh_proto::EnrolmentState::None | mesh_proto::EnrolmentState::Expired => {
            println!();
            println!(
                "{}",
                term::out()
                    .prose("Run `meshnode enrol` to request access, or send your key to an admin.")
            );
        }
        mesh_proto::EnrolmentState::Pending => {
            println!();
            println!(
                "{}",
                term::out().prose(
                    "Show the code above to a mesh admin; `meshnode enrol` waits for the answer."
                )
            );
        }
        mesh_proto::EnrolmentState::Declined => {
            println!();
            println!(
                "{}",
                term::out().prose("Talk to a mesh admin. `meshnode enrol` asks again.")
            );
        }
        mesh_proto::EnrolmentState::Enrolled => {
            if info.is_daemon_key && info.is_founder {
                println!();
                println!(
                    "{}",
                    term::out().prose(
                        "This is the daemon's own admin key, so this machine is an admin with no \
                     setup.\nEveryone else enrols with `meshnode enrol` — worth trying once to \
                     see what they see."
                    )
                );
            }
        }
    }
    Ok(())
}

/// A grant as a sentence rather than two globs (M23 §5).
///
/// `Operator, on nodes tagged dc=east, for services matching deploy-*,
/// until 14 March` — the scoping fields are powerful and almost never used,
/// and rendering them as fields makes every unscoped grant look like it has
/// two settings somebody chose.
pub fn describe_grant(
    role: &str,
    nodes: &str,
    services: &str,
    expires_at_micros: Option<u64>,
) -> String {
    let mut out = capitalize(role);
    match nodes {
        "" | "any" => {}
        n => match n.strip_prefix("tag=") {
            Some(tag) => out.push_str(&format!(", on nodes tagged {tag}")),
            None => match n.strip_prefix("name=") {
                Some(name) => out.push_str(&format!(", on {name}")),
                None => out.push_str(&format!(", on nodes matching {n}")),
            },
        },
    }
    if services != "*" && !services.is_empty() {
        out.push_str(&format!(", for services matching {services}"));
    }
    if let Some(at) = expires_at_micros {
        out.push_str(&format!(", until {}", date_only(at)));
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `2026-03-14`. Deliberately not a time: a grant that ends "at 14:32:07"
/// invites somebody to plan around the seconds.
fn date_only(micros: u64) -> String {
    let days = (micros / 1_000_000) / 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), so this needs no
    // calendar crate for the one place a date is nicer than a duration.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `~/.meshware/identity/user.key` rather than the absolute path, when the
/// path is under the user's home — the short form is the one people
/// recognise as theirs.
fn tilde(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.display().to_string();
    };
    let home = std::path::PathBuf::from(home);
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// `meshnode definitions` (M24 §3.1).
///
/// The list is not news to the engine — every row already replicated
/// automatically. It is news to the *person*, who had no way to see that
/// publishing a collector once had put it on four hundred machines.
pub fn print_definitions(
    rows: &[mesh_proto::DefinitionRow],
    nodes: &[NodeRow],
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<18} {:>7}  {:<28} REPLICATED BY",
            "WHAT", "COUNT", "LAST CHANGE"
        ))
    );
    for r in rows {
        let last = match (&r.last_changed_what, r.last_changed_micros) {
            (Some(what), Some(at)) => format!("{} {}", truncate(what, 18), micros_ago(now, at)),
            _ => "-".into(),
        };
        println!(
            "{} {:>7}  {} {}{}",
            term::out().bold(format!("{:<18}", r.kind)),
            r.count,
            term::out().dim(format!("{:<28}", last)),
            r.op,
            if r.executes {
                "  ·  runs on nodes"
            } else {
                ""
            }
        );
    }

    // Catch-up, from the sync reading the node view already computes. A
    // node that is caught up has every definition by construction, so this
    // is the whole of "how far has it spread" — and it is deliberately
    // phrased as *this node's* observation, because a gossip mesh has no
    // vantage point from which to claim more.
    let peers: Vec<&NodeRow> = nodes.iter().filter(|n| !n.is_self && !n.left).collect();
    let caught_up = peers
        .iter()
        .filter(|n| n.sync.is_some_and(|s| s.behind == 0))
        .count();
    let unknown = peers.iter().filter(|n| n.sync.is_none()).count();
    println!();
    if peers.is_empty() {
        println!("This is the only node, so everything above is already everywhere it goes.");
    } else {
        println!(
            "{caught_up} of {} other nodes had everything at their last sync with this one{}.",
            peers.len(),
            if unknown > 0 {
                format!("; {unknown} have not asked since this daemon started")
            } else {
                String::new()
            }
        );
        for n in peers {
            let Some(sync) = n.sync else { continue };
            if sync.behind > 0 {
                println!(
                    "  {:<20} {} ops behind, {} ago",
                    truncate(&n.name, 20),
                    sync.behind,
                    micros_ago(now, now.saturating_sub(sync.age_micros))
                );
            }
        }
    }
    println!();
    println!(
        "{}",
        term::out().prose(
            "Bundle *content* does not replicate — push it with `meshnode bundles prefetch`."
        )
    );
    Ok(())
}

/// `meshnode upgrade --status` (M24 §4): what the fleet is running.
///
/// The question a thousand-node fleet actually has is not "what does this
/// node run" but **"which of them are behind, and how many"** — and until
/// nodes announced `cap.ver` there was no way to ask it short of visiting
/// each one. Grouped by version, because a list of a thousand rows is not
/// an answer.
pub fn print_fleet_versions(rows: &[NodeRow], target: Option<&str>) -> Result<()> {
    use std::collections::BTreeMap;

    let live: Vec<&NodeRow> = rows.iter().filter(|n| !n.left && !n.revoked).collect();
    if live.is_empty() {
        println!("no nodes");
        return Ok(());
    }
    let target = target.map(|t| t.trim().trim_start_matches(['v', 'V']).to_string());

    let mut by_version: BTreeMap<String, Vec<&NodeRow>> = BTreeMap::new();
    for n in &live {
        by_version
            .entry(n.version.clone().unwrap_or_else(|| "unknown".into()))
            .or_default()
            .push(n);
    }
    // Newest first, with `unknown` last: it is a state to chase, not a
    // version to sort among versions.
    let mut versions: Vec<(&String, &Vec<&NodeRow>)> = by_version.iter().collect();
    versions.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
        ("unknown", _) => std::cmp::Ordering::Greater,
        (_, "unknown") => std::cmp::Ordering::Less,
        _ => version_cmp(b.0, a.0),
    });

    println!(
        "{}",
        term::out().dim(format!("{:<12} {:>6}  NODES", "VERSION", "COUNT"))
    );
    for (version, nodes) in &versions {
        let mark = match (&target, version.as_str()) {
            (Some(t), v) if v == t => "  ← target",
            (Some(_), "unknown") => "  ← never announced a version",
            (Some(_), _) => "  ← outdated",
            (None, _) => "",
        };
        let mut names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        let shown = if names.len() > 6 {
            format!("{} and {} more", names[..6].join(", "), names.len() - 6)
        } else {
            names.join(", ")
        };
        println!(
            "{} {:>6}  {}{}",
            term::out().bold(format!("{:<12}", version)),
            nodes.len(),
            truncate(&shown, 60),
            mark
        );
    }

    if let Some(t) = &target {
        let behind: Vec<&&NodeRow> = live
            .iter()
            .filter(|n| n.version.as_deref() != Some(t.as_str()))
            .collect();
        println!();
        if behind.is_empty() {
            println!("Every node runs {t}.");
        } else {
            println!(
                "{} of {} node{} are not on {t}.",
                behind.len(),
                live.len(),
                if live.len() == 1 { "" } else { "s" }
            );
            // Ring-shaped advice, because `--on any` is refused: the tags
            // people already have are the rings they already have.
            let mut tags: Vec<&str> = behind
                .iter()
                .flat_map(|n| n.tags.iter().map(String::as_str))
                .collect();
            tags.sort_unstable();
            tags.dedup();
            match tags.first() {
                Some(tag) => println!(
                    "  Roll a ring first:  meshnode upgrade --on tag={tag} --to {t} --batch 8"
                ),
                None => println!(
                    "{}",
                    term::out().prose(
                        "  Tag a canary ring first (`meshnode nodes tag --add canary` on a few), \
                     then roll it."
                    )
                ),
            }
        }
    } else {
        println!();
        println!("Add --to <version> to see what is outdated against it.");
    }
    Ok(())
}

/// One comparison for the whole workspace (`mesh_proto::version_cmp`):
/// semver, with pre-releases below the release they precede.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    mesh_proto::version_cmp(a, b)
}

/// `meshnode upgrade --releases` (M24 §9).
///
/// The catalogue of binaries this mesh can install without reaching
/// anything outside itself. `HOLDERS` is the number that decides whether a
/// rollout waits on a download: warm it with `meshnode bundles prefetch`
/// and the swap is instant everywhere.
pub fn print_releases(rows: &[mesh_proto::ReleaseRow]) -> Result<()> {
    if rows.is_empty() {
        println!("no releases published into this mesh");
        println!();
        println!("Put one in with:  meshnode upgrade --publish <binary> --version <X>");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    // Y6. The column appears once anything in this mesh has been
    // co-signed, so a fleet that has not opted in reads exactly as before —
    // and one that has can see at a glance which releases are installable.
    let any_cosign = rows.iter().any(|r| r.cosigned_by.is_some());
    let mut header = vec!["VERSION", "ARCH", "BUNDLE", "SIZE", "HOLDERS", "HERE"];
    if any_cosign {
        header.push("CO-SIGNED");
    }
    header.push("PUBLISHED");
    let mut t = table(&header, &[3, 4]);
    for r in rows {
        let mut row = vec![
            r.version.clone(),
            if r.arch.is_empty() {
                "-".into()
            } else {
                r.arch.clone()
            },
            r.bundle_id.short(),
            human_bytes(r.total_size),
            r.holders.to_string(),
            if r.local { "yes" } else { "-" }.to_string(),
        ];
        if any_cosign {
            row.push(match &r.cosigned_by {
                None => "NO".to_string(),
                Some(_) if r.cosign_user_present => "yes, touched".to_string(),
                Some(_) => "yes".to_string(),
            });
        }
        row.push(format!(
            "{} by {}",
            micros_ago(now, r.published_at_micros),
            r.published_by
        ));
        t.add_row(row);
    }
    println!("{t}");
    if any_cosign && rows.iter().any(|r| r.cosigned_by.is_none()) {
        println!();
        println!(
            "A release marked NO has a valid minisign signature and no hardware co-signature."
        );
        println!("On a mesh that trusts a co-signing key, that release will not install anywhere.");
    }
    println!();
    println!("Nodes fetch these from each other. Warm a ring before rolling it:");
    println!("  meshnode bundles prefetch <bundle> --on tag=<ring>");
    Ok(())
}

/// `meshnode upgrade --plans` (M24 §4).
///
/// A plan is a standing authorization, not a rollout in progress — nothing
/// acts on one until a driver asks a node to. Listing them answers "what is
/// this mesh currently willing to be upgraded to, and by whom".
pub fn print_upgrade_plans(rows: &[mesh_proto::UpgradePlanRow]) -> Result<()> {
    if rows.is_empty() {
        println!("no upgrade plans");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    let mut t = table(&["PLAN", "VERSION", "RING", "CREATED", "STATE", "BY"], &[]);
    for r in rows {
        t.add_row(vec![
            r.plan_id.to_display_string(),
            if r.force {
                format!("{} (forced)", r.version)
            } else {
                r.version.clone()
            },
            r.selector.clone(),
            micros_ago(now, r.created_at_micros),
            if r.cancelled {
                "withdrawn"
            } else if r.live {
                "live"
            } else {
                "expired"
            }
            .to_string(),
            r.created_by.clone(),
        ]);
    }
    println!("{t}");
    println!();
    println!(
        "{}",
        term::out()
            .prose("A plan authorizes; it does not run. Drive one with `meshnode upgrade --on …`.")
    );
    Ok(())
}

/// What this mesh — and this node — verify releases against (M24 §9).
pub fn print_release_keys(view: &mesh_proto::ReleaseKeyView) -> Result<()> {
    let now = mesh_proto::now_micros();
    if view.keys.is_empty() {
        println!("this mesh trusts no release key");
        println!();
        if view.embedded_is_placeholder {
            println!(
                "Nothing has been published yet. Publishing a release signs it with your own \
                 release key, and trusts that key mesh-wide the first time:"
            );
            println!("  meshnode upgrade --publish ./meshnode --version <v>");
            println!();
            println!("Or trust a key you already sign builds with:");
        } else {
            println!(
                "Releases verify against the key compiled into each binary. To sign your own:"
            );
        }
        println!("  meshnode upgrade --trust-key release.pub");
        return Ok(());
    }
    let mut t = table(&["KEY", "ADDED", "STATE", "LABEL"], &[]);
    for k in &view.keys {
        t.add_row(vec![
            k.pubkey.clone(),
            micros_ago(now, k.added_at_micros),
            if k.revoked {
                "revoked"
            } else if k.in_use_here {
                "trusted"
            } else {
                "trusted*"
            }
            .to_string(),
            k.label.clone(),
        ]);
    }
    println!("{t}");
    if let Some(local) = &view.local_override {
        println!();
        println!(
            "{}",
            term::out().prose(&format!(
                "* this node has `[release] pubkey` set ({}…), which overrides the mesh's list \
             here — the machine owner's written-down answer wins locally.",
                local.chars().take(12).collect::<String>()
            ))
        );
    }
    Ok(())
}

/// The pending enrolment queue (`meshnode users pending`, M23 §3.2).
pub fn print_enrolments(rows: &[mesh_proto::EnrolmentRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nobody is waiting to join");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<20} {:<16} {:<12} KEY",
            "NAME", "CODE", "FROM", "ASKED"
        ))
    );
    for r in rows {
        let name = if r.suggested_username.is_empty() {
            "(unnamed)"
        } else {
            &r.suggested_username
        };
        println!(
            "{} {:<20} {:<16} {} {}",
            term::out().bold(format!("{:<16}", truncate(name, 16))),
            r.code,
            truncate(&r.from_node, 16),
            term::out().dim(format!("{:<12}", micros_ago(now, r.requested_at_micros))),
            r.user_id.short()
        );
        // What a provider proved, when the person logged in first (E1-P5).
        // Indented under the row rather than given columns: it is absent
        // for most requests, and a mostly-empty column costs every row.
        if let Some(v) = &r.verified {
            let who = if v.display_name.is_empty() {
                v.subject.clone()
            } else {
                format!("{} <{}>", v.display_name, v.email)
            };
            println!(
                "{} ✓ {who} — verified by {} via {}{}",
                term::label_w("", 16),
                v.provider,
                v.verified_by,
                if v.fresh {
                    ""
                } else {
                    "  (STALE — ask them to log in again)"
                }
            );
            if !v.groups.is_empty() {
                println!(
                    "{}   groups: {}",
                    term::label_w("", 16),
                    v.groups.join(", ")
                );
            }
            if let Some(role) = &v.suggested_role {
                println!("{}   suggests --role {role}", term::label_w("", 16));
            }
        }
        if r.declined {
            println!(
                "{} declined{}",
                term::label_w("", 16),
                match &r.declined_by {
                    Some(by) => format!(" by {by}"),
                    None => String::new(),
                }
            );
        }
    }
    println!();
    println!(
        "{}",
        term::out().prose("Approve with `meshnode users approve <code> --role <role>`.")
    );
    Ok(())
}

pub fn print_users(rows: &[UserRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no users");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    // Y2/Y3. The column appears only once somebody has attested a key, so
    // a mesh that has never heard of hardware identity reads exactly as it
    // always did — and a mesh where one admin has is visibly a mesh where
    // the others have not, which is the column doing its job.
    let any_hardware = rows.iter().any(|r| r.hardware.is_some());
    let mut header = vec!["USER", "ID", "ROLES", "STATE"];
    if any_hardware {
        header.push("HARDWARE");
    }
    header.push("ADDED");
    let mut t = table(&header, &[]);
    for r in rows {
        let state = if r.revoked {
            "revoked"
        } else if r.is_founder {
            "founder"
        } else {
            "active"
        };
        let mut row = vec![
            r.username.clone(),
            r.user_id.short(),
            r.roles.join(","),
            state.to_string(),
        ];
        if any_hardware {
            row.push(match &r.hardware {
                // A dash, not an empty cell: "this key is on a disk" is an
                // answer, and a blank reads as "not checked".
                None => "—".to_string(),
                Some(h) if h.revoked => "withdrawn".to_string(),
                Some(h) => format!("{} {}", h.kind, h.slot),
            });
        }
        row.push(micros_ago(now, r.created_at_micros));
        t.add_row(row);
    }
    println!("{t}");
    if any_hardware {
        println!();
        println!(
            "HARDWARE means the mesh verified a certificate chain to an authority it trusts: \
             that key was"
        );
        println!(
            "{}",
            term::out().prose(
                "generated inside the device and cannot be exported. `meshnode users attest-root` \
             lists the authorities."
            )
        );
    }
    Ok(())
}

/// `meshnode users attest-root` (Y2/Y3).
pub fn print_attest_roots(rows: &[mesh_proto::AttestRootRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("this mesh trusts no attestation authorities");
        println!();
        println!("Until it trusts one, no hardware attestation verifies and no HARDWARE column");
        println!("appears — an unverified chain is worth less than no chain, because it looks");
        println!("like proof.");
        println!();
        println!("  meshnode users attest-root add ca.der --label \"Yubico PIV Attestation CA\"");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    let mut t = table(
        &["AUTHORITY", "ID", "KEYS", "STATE", "ADDED BY", "ADDED"],
        &[],
    );
    for r in rows {
        t.add_row(vec![
            r.label.clone(),
            r.id.chars().take(12).collect(),
            r.attesting.to_string(),
            if r.removed { "removed" } else { "trusted" }.to_string(),
            r.added_by_username.clone(),
            micros_ago(now, r.added_at_micros),
        ]);
    }
    println!("{t}");
    Ok(())
}

/// Identity providers this mesh believes (E1-P5).
pub fn print_oidc_providers(rows: &[mesh_proto::OidcProviderRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no identity providers");
        println!();
        println!("A mesh with none works exactly as it always did — enrolment is unchanged.");
        println!("Configure one to let people prove who they are before an admin approves them:");
        println!("  meshnode oidc providers put entra --issuer … --client-id … \\");
        println!("      --jwks-uri … --device-endpoint … --token-endpoint …");
        return Ok(());
    }
    let mut t = table(
        &["NAME", "ISSUER", "CLIENT", "GROUPS CLAIM", "SUGGESTS"],
        &[],
    );
    for r in rows {
        t.add_row(vec![
            r.name.clone(),
            r.issuer.clone(),
            r.client_id.clone(),
            r.groups_claim.clone(),
            if r.group_roles.is_empty() {
                "-".to_string()
            } else {
                r.group_roles.join(" ")
            },
        ]);
    }
    println!("{t}");
    Ok(())
}

/// Machine principals (E1-P4).
///
/// `EXPIRES` is the column that matters and is why this is not folded into
/// `print_users`: for a person the interesting states are founder/active/
/// revoked, and for a machine it is *how long is left*. A row whose clock
/// has run out says so in the state column and shows no roles, because
/// `roles_at` really does return none for it — the table is not describing
/// the grant, it is describing what the principal can do.
pub fn print_service_accounts(rows: &[mesh_proto::ServiceAccountRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no service accounts");
        println!();
        println!("A pipeline should not authenticate as a person. Mint one with:");
        println!("  meshnode serviceaccounts create ci-deploy --role developer --expires-in 90d");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    let mut t = table(&["NAME", "ID", "ROLES", "STATE", "EXPIRES", "FOR"], &[]);
    for r in rows {
        let state = if r.revoked {
            "revoked"
        } else if r.expired {
            "expired"
        } else {
            "active"
        };
        let expires = match r.expires_at_micros {
            None => "never".to_string(),
            Some(e) if r.expired => format!("{} (past)", crate::format::micros_ago(now, e)),
            Some(e) => humantime::format_duration(std::time::Duration::from_secs(
                e.saturating_sub(now) / 1_000_000,
            ))
            .to_string(),
        };
        t.add_row(vec![
            r.name.clone(),
            r.user_id.short(),
            if r.roles.is_empty() {
                "-".to_string()
            } else {
                r.roles.join(",")
            },
            state.to_string(),
            expires,
            r.description.clone(),
        ]);
    }
    println!("{t}");
    Ok(())
}

pub fn print_grants(rows: &[GrantRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no grants");
        return Ok(());
    }
    let mut t = table(
        &["GRANT", "USER", "ROLE", "NODES", "SERVICES", "STATE"],
        &[],
    );
    for r in rows {
        let state = if r.revoked {
            "revoked"
        } else if r.holder_revoked {
            // The grant itself is untouched; the person holding it is gone,
            // and `roles_at` has always known that. Printing "active" here
            // was the table disagreeing with the authorization (E2.3) — the
            // kind of wrong that gets believed in an access review.
            "holder revoked"
        } else if r.expired {
            "expired"
        } else {
            "active"
        };
        t.add_row(vec![
            r.grant_id.to_string(),
            r.username.clone(),
            r.role.to_string(),
            r.nodes.clone(),
            r.services.clone(),
            state.to_string(),
        ]);
    }
    println!("{t}");
    Ok(())
}

pub fn print_secrets(
    rows: &[SecretRow],
    scopes: &[mesh_proto::SecretScopeRow],
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "secrets": rows,
                "scopes": scopes,
            }))?
        );
        return Ok(());
    }
    let live: Vec<&SecretRow> = rows.iter().filter(|r| !r.removed).collect();
    if live.is_empty() {
        println!("no secrets");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<20} {:<4} {:<12} {:<12} {:<8} SCOPE",
            "NAME", "VER", "BY", "DECRYPTABLE", "SET"
        ))
    );
    for r in live {
        // Who may use it and where — the part a reader has to know is
        // "anyone who can run a job there" when nothing narrows it (SEC-7).
        let scope = scopes.iter().find(|s| s.name == r.name);
        let mut says = Vec::new();
        if let Some(s) = scope {
            if !s.nodes.is_empty() {
                says.push(format!("on {}", mesh_proto::text::clean(&s.nodes, 64)));
            }
            let who: Vec<String> = s
                .users
                .iter()
                .map(|u| mesh_proto::text::clean(u, mesh_proto::text::MAX_NAME_CHARS))
                .chain(s.roles.iter().map(|r| format!("role:{r}")))
                .collect();
            if !who.is_empty() {
                says.push(format!("for {}", who.join(",")));
            }
        }
        if says.is_empty() {
            says.push("default nodes, any runner".into());
        }
        println!(
            "{} {:<4} {:<12} {:<12} {:<8} {}",
            term::out().bold(format!("{:<20}", truncate(&r.name, 20))),
            r.version,
            truncate(&r.created_by_name, 12),
            format!("{}/{} nodes", r.wrapped_for, r.members),
            micros_ago(now, r.created_at_micros),
            says.join("; ")
        );
    }
    Ok(())
}

pub fn print_providers(rows: &[ProviderRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no secret providers configured on this node");
        println!(
            "{}",
            term::out()
                .prose("(add [secrets.providers.<name>] with a `plugin` path to meshnode.toml)")
        );
        return Ok(());
    }
    println!("{}", term::out().dim(format!("{:<16} PLUGIN", "PROVIDER")));
    for r in rows {
        println!(
            "{} {}",
            term::out().bold(format!("{:<16}", truncate(&r.name, 16))),
            r.plugin
        );
    }
    Ok(())
}

/// `mode`: table | json | jsonl.
/// Audit rows, marking the rows whose actor is a **machine** (E1-P4).
///
/// The join is done in the client, on purpose. Actor type is a projection
/// over replicated state rather than a field on [`AuditEntry`] — whose
/// postcard encoding is signed into every audit op ever written — so the
/// alternative was widening the `Audit` IPC response and, with it, the
/// generated SDK types and Director's decoder, to carry a fact the caller
/// can already ask for in one extra request. It is one call to
/// `ServiceAccounts` and a set lookup.
///
/// `machines` empty renders exactly as the table always did.
pub fn print_audit_with_actors(
    entries: &[AuditEntry],
    mode: &str,
    machines: &[mesh_proto::UserId],
) -> Result<()> {
    match mode {
        "json" => {
            println!("{}", serde_json::to_string_pretty(entries)?);
            return Ok(());
        }
        "jsonl" => {
            let mut out = std::io::stdout().lock();
            for e in entries {
                use std::io::Write;
                match writeln!(out, "{}", serde_json::to_string(e)?) {
                    // `meshnode audit --output jsonl | head` is a normal
                    // workflow; a closed pipe is not an error.
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                    other => other?,
                }
            }
            return Ok(());
        }
        _ => {}
    }
    if entries.is_empty() {
        println!("no audit entries");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    let ink = term::out();
    println!(
        "{}",
        ink.dim(format!(
            "{:<9} {:<12} {:<16} {:<20} {:<10} DETAIL",
            "WHEN", "USER", "ACTION", "TARGET", "OUTCOME"
        ))
    );
    for e in entries {
        // A trailing marker rather than a column: the answer is "machine"
        // for a small minority of rows, and a mostly-empty column costs
        // every other row width it needs for `detail`.
        let who = if machines.contains(&e.user) {
            format!("{}*", truncate(&e.username, 11))
        } else {
            truncate(&e.username, 12)
        };
        // Padded, then painted: see `table`.
        let target = format!("{:<20}", truncate(&e.target, 20));
        println!(
            "{} {:<12} {} {} {} {}",
            ink.dim(format!("{:<9}", micros_ago(now, e.at_micros))),
            who,
            ink.bold(format!("{:<16}", truncate(&e.action, 16))),
            if term::looks_like_id(&target) {
                ink.dim(target)
            } else {
                target
            },
            ink.state(format!("{:<10}", truncate(&e.outcome, 10))),
            ink.dim(truncate(&e.detail, 34)),
        );
    }
    if entries.iter().any(|e| machines.contains(&e.user)) {
        println!();
        println!(
            "{}",
            ink.dim("* a service account — a machine principal, not a person")
        );
    }
    Ok(())
}

// -- history (M14 §2) --------------------------------------------------------

/// A duration a person reads at a glance: `1.4s`, `2m 30s`, `1h 05m`.
///
/// Never bare seconds past a minute. "412s" makes a reader do arithmetic to
/// find out whether their deploy is slow, and the answer to "how long did
/// this take" should not require arithmetic.
pub fn duration_micros(micros: u64) -> String {
    let secs = micros / 1_000_000;
    if secs == 0 {
        return format!("{}ms", (micros / 1000).max(1));
    }
    if secs < 10 {
        return format!("{:.1}s", micros as f64 / 1_000_000.0);
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
}

/// An absolute instant, in the local calendar, to the minute. History is
/// read across days, and "3d ago" stops being useful the moment somebody
/// needs to correlate a run with an incident timeline.
pub fn stamp(micros: u64) -> String {
    if micros == 0 {
        return "-".into();
    }
    let secs = (micros / 1_000_000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Days since the Unix epoch to a civil date. Howard Hinnant's algorithm,
/// the same one `mesh_core::tasks` carries — a calendar is not worth a
/// dependency in a binary whose size is a feature.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn print_history_jobs(page: &mesh_proto::HistoryJobPage, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(page)?);
        return Ok(());
    }
    if page.rows.is_empty() {
        // See `print_history_runs`: a known absence is not an empty result.
        match page.absence.note() {
            Some(note) => println!("no jobs in that window.\n\n{note}"),
            None => println!("no jobs matched"),
        }
        return Ok(());
    }
    let ink = term::out();
    println!(
        "{}",
        ink.dim(format!(
            "{:<18} {:<34} {:<10} {:<6} {:<10} FINISHED",
            "JOB", "COMMAND", "STATE", "EXIT", "TOOK"
        ))
    );
    for r in &page.rows {
        // Padded, then painted: see `table`.
        let exit = format!(
            "{:<6}",
            r.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "{} {:<34} {} {} {} {}",
            ink.dim(format!("{:<18}", r.job_id.short())),
            truncate(&r.cmd_preview, 34),
            ink.state(format!("{:<10}", truncate(r.state.as_str(), 10))),
            if r.exit_code.is_none() {
                ink.dim(exit)
            } else {
                exit
            },
            ink.dim(format!(
                "{:<10}",
                r.duration_micros()
                    .map(duration_micros)
                    .unwrap_or_else(|| "-".into())
            )),
            ink.dim(stamp(r.finished_at_micros.unwrap_or(r.created_at_micros))),
        );
    }
    match page.total {
        Some(total) if total as usize > page.rows.len() => {
            println!(
                "\n{} of {total} matching jobs — page with --offset",
                page.rows.len()
            );
        }
        Some(_) => {}
        None => println!(
            "\n{} jobs (more matched than were counted)",
            page.rows.len()
        ),
    }
    if let Some(note) = page.absence.note() {
        println!("\n{note}");
    }
    Ok(())
}

pub fn print_history_stats(s: &mesh_proto::HistoryStats, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    println!("{}   {}", term::label("indexed jobs"), s.jobs);
    println!("{}     {}", term::label("index size"), bytes(s.index_bytes));
    match (s.oldest_micros, s.newest_micros) {
        (Some(old), Some(new)) => {
            println!("{}         {}", term::label("oldest"), stamp(old));
            println!("{}         {}", term::label("newest"), stamp(new));
            let span_days = (new.saturating_sub(old)) / (86_400 * 1_000_000);
            println!("{}           {span_days} days", term::label("span"));
        }
        _ => println!("{}          (nothing indexed yet)", term::label("range")),
    }
    Ok(())
}

/// `meshnode config paths` — the whole layout, one screen.
pub fn print_paths(rows: &[(&'static str, std::path::PathBuf, bool)], json: bool) -> Result<()> {
    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|(label, path, node)| {
                serde_json::json!({
                    "name": label,
                    "path": path,
                    "exists": path.exists(),
                    "node_needs_it": node,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!("{:<14} {:<8} PATH", "WHAT", "STATE"))
    );
    for (label, path, node_needs) in rows {
        let state = if path.exists() { "present" } else { "-" };
        let note = if *node_needs { "" } else { "  (Director)" };
        let ink = term::out();
        println!(
            "{} {} {}{}",
            ink.bold(format!("{label:<14}")),
            ink.state(format!("{state:<8}")),
            path.display(),
            ink.dim(note)
        );
    }
    Ok(())
}

/// The answer to a job or service `--dry-run`.
pub fn print_job_preview(p: &mesh_proto::JobPreview) -> Result<()> {
    println!("{} — dry run, nothing dispatched", p.action);
    if let (Some(name), Some(v)) = (&p.service, p.version) {
        println!("{}     {name}@{v}", term::label("service"));
    }
    if let Some(b) = &p.bundle_id {
        println!("{}      {}", term::label("bundle"), b.short());
    }
    if let Some(rt) = &p.runtime {
        println!("{}     {rt}", term::label("runtime"));
    }
    println!("{}    {}", term::label("priority"), p.priority);
    if let Some(t) = p.timeout_secs {
        println!("{}     {t}s", term::label("timeout"));
    }
    println!(
        "{}          {}  →  {}",
        term::label("on"),
        p.selector,
        if p.targets.is_empty() {
            "nothing matches".to_string()
        } else {
            p.targets.join(", ")
        }
    );
    if !p.excluded.is_empty() {
        println!(
            "{}    {} (your grants do not cover)",
            term::label("excluded"),
            p.excluded.join(", ")
        );
    }
    if !p.env_keys.is_empty() {
        println!("{}         {}", term::label("env"), p.env_keys.join(", "));
    }
    if !p.secrets.is_empty() {
        // Names, never values (§5.2) — a preview is a thing people paste
        // into tickets.
        println!(
            "{}     {} (names only)",
            term::label("secrets"),
            p.secrets.join(", ")
        );
    }
    println!(
        "{}       {} node(s) would be touched",
        term::label("blast"),
        p.blast_radius
    );
    Ok(())
}

/// Wrap prose to a readable measure, leaving indented lines alone.
///
/// A remedy is prose with shell commands in it, and the two want opposite
/// treatment: the prose should wrap to the reader's terminal, and the
/// commands must not — a wrapped command line is a command that cannot be
/// copied. The convention is the one the messages are written to: a line
/// beginning with whitespace is verbatim, everything else is a paragraph.
fn reflow(body: &str) -> String {
    const MEASURE: usize = 86;
    let mut out = Vec::new();
    for para in body.split('\n') {
        if para.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        if para.starts_with(' ') || para.starts_with('\t') {
            out.push(para.to_string());
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > MEASURE {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// Schedules (M8 §3.9)
// ---------------------------------------------------------------------------

/// `meshnode sessions` (M17 §3.5).
///
/// The direction column is the one that matters and the one a reader will
/// not guess: `in` is somebody shelled into this machine, `out` is this
/// machine reaching somewhere else. They are very different facts and they
/// live in the same table because they are the same object seen from
/// opposite ends.
pub fn print_sessions(rows: &[SessionRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no live sessions");
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<6} {:<4} {:<8} {:<14} {:<12} {:<8} {:<10} REASON",
            "ID", "DIR", "KIND", "PEER", "USER", "AGE", "TRAFFIC"
        ))
    );
    for r in rows {
        println!(
            "{} {:<4} {:<8} {:<14} {:<12} {} {:<10} {}",
            term::out().dim(format!("{:<6}", r.id)),
            if r.hosting { "in" } else { "out" },
            truncate(&r.kind, 8),
            truncate(&r.peer_name, 14),
            truncate(&r.username, 12),
            term::out().dim(format!("{:<8}", micros_ago(now, r.opened_at_micros))),
            format!("{}/{}", human_bytes(r.bytes_in), human_bytes(r.bytes_out)),
            truncate(&r.reason, 40)
        );
    }
    Ok(())
}

/// `meshnode sessions policy ls` (M17 §3.4).
///
/// The `HERE` column is the one people came for: it answers *why can I not
/// get a prompt on this box* without anybody having to mentally evaluate a
/// selector.
pub fn print_session_policies(rows: &[SessionPolicyRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "{}",
            term::out().prose(
                "no session policies — nobody can open a terminal anywhere in this mesh.\n\
             `meshnode sessions policy put dev --users '*' --nodes tag=dev --allow shell`"
            )
        );
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<12} {:<16} {:<20} {:<5} {:<10} BY",
            "POLICY", "USERS", "NODES", "ALLOWS", "HERE", "EXPIRES"
        ))
    );
    for r in rows {
        let expires = match r.expires_at_micros {
            None => "never".to_string(),
            Some(at) if at <= now => "expired".to_string(),
            Some(at) => format!("in {}", micros_ago(at, now)),
        };
        println!(
            "{} {:<12} {:<16} {:<20} {:<5} {} {}",
            term::out().bold(format!("{:<16}", truncate(&r.id, 16))),
            truncate(&r.users, 12),
            truncate(&r.nodes, 16),
            truncate(&r.allow.join(","), 20),
            if r.covers_this_node { "yes" } else { "-" },
            term::out().dim(format!("{:<10}", expires)),
            truncate(&r.created_by_username, 14)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Op-log state (C0)
// ---------------------------------------------------------------------------

fn bytes_human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn horizon_human(secs: u64) -> String {
    match secs {
        0 => "none".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// `meshnode sinks` — what every sink is doing (M31 B1 item 9).
///
/// Reads like `meshnode cache stats`, and the columns are chosen against
/// L1–L5's rule that a number nobody can see is a number nobody trusts.
/// **The last status is verbatim**: `503` sends an operator to the right
/// machine, "sink unhealthy" sends them to the wrong one.
pub fn print_sinks(rows: &[mesh_proto::SinkRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        // Not an empty table. Almost no mesh should have a sink, and the
        // absence of one is the healthy state rather than a gap to fill.
        println!("No sinks. This mesh keeps its own history and needs nothing else running.");
        println!(
            "{}",
            term::out()
                .prose("`meshnode sinks add --help` if you need a copy somewhere you already own.")
        );
        return Ok(());
    }

    // The latch outranks the table: while it is on, every other number reads
    // as broken and the fix is one command.
    if rows.iter().any(|r| r.paused_by_restore) {
        println!(
            "{}",
            term::out().prose(
                "This node's data was restored from a backup, so nothing is exporting.\n\
             Review the sinks below, then `meshnode sinks resume`.\n"
            )
        );
    }

    let w = |f: fn(&mesh_proto::SinkRow) -> usize, min: usize| {
        rows.iter().map(f).max().unwrap_or(min).max(min)
    };
    let nw = w(|r| r.name.chars().count(), 4);
    let kw = w(|r| r.kind.chars().count(), 4);
    let tw = w(|r| r.target.chars().count().min(44), 6);

    println!(
        "{:<nw$}  {:<kw$}  {:<tw$}  {:>8}  {:>8}  {:>8}  {:>8}  LAST",
        "NAME", "KIND", "TARGET", "LAG", "SENT", "SPOOLED", "DROPPED"
    );
    for r in rows {
        let target: String = if r.target.chars().count() > tw {
            r.target
                .chars()
                .skip(r.target.chars().count() - tw + 1)
                .collect::<String>()
        } else {
            r.target.clone()
        };
        println!(
            "{:<nw$}  {:<kw$}  {:<tw$}  {:>8}  {:>8}  {:>8}  {:>8}  {}",
            r.name,
            r.kind,
            target,
            // A row whose counters are not this node's is drawn as
            // *unknown*, never as zero. The cursor and spool live on the
            // exporting node, so printing this node's empty copy of them
            // is how a sink that has been failing every tick for an hour
            // reads as healthy from everywhere except the one machine that
            // knows better.
            if !r.enabled {
                "off".into()
            } else if r.status_is_local {
                lag_human(r.lag_secs)
            } else {
                "·".into()
            },
            if r.status_is_local {
                r.sent.to_string()
            } else {
                "·".into()
            },
            if r.status_is_local {
                bytes_human(r.spooled_bytes)
            } else {
                "·".into()
            },
            if r.status_is_local {
                r.dropped.to_string()
            } else {
                "·".into()
            },
            if !r.status_is_local {
                "· counters are on the exporting node"
            } else if r.last_status.is_empty() {
                "—"
            } else {
                &r.last_status
            }
        );
    }

    // Every line below the table is an instruction rather than a reading.
    //
    // The export gap comes first: it is the only number here that means
    // history has been lost *on purpose*, and an operator finding that out
    // from a query six months later is what §18 decision 03 exists to
    // prevent.
    for r in rows.iter().filter(|r| r.forced_gaps > 0) {
        println!(
            "\n{}: {} record(s) were deleted before this sink took them. Its \
             {}-day hold\nexpired while the destination was unreachable, so the mesh kept \
             deleting — the\nexport gap is **permanent**. Those rows are not coming back.",
            r.name, r.forced_gaps, r.hold_for_export_days
        );
    }
    for r in rows.iter().filter(|r| r.payloads_too_large > 0) {
        println!(
            "\n{}: {} job output(s) were over this sink's size cap. The record carries the \
             reference;\nthe bytes stayed on the node that produced them.",
            r.name, r.payloads_too_large
        );
    }
    let dropped: u64 = rows.iter().map(|r| r.dropped).sum();
    if dropped > 0 {
        println!(
            "\n{dropped} batch(es) have been dropped because a spool filled. **Those rows are \
             gone**\nand will not be re-sent. Raise --spool-mb, or fix the destination."
        );
    }
    for r in rows.iter().filter(|r| r.enabled && r.elected.is_empty()) {
        println!(
            "{}",
            term::out().prose(&format!(
                "\n{}: no live node is eligible to export it. Check its `run_on`, and that an \
             eligible\nnode runs a build new enough to have sinks.",
                r.name
            ))
        );
    }
    if let Some(here) = rows.iter().find(|r| r.elected_here) {
        println!(
            "{}",
            term::out().prose(&format!(
                "\nThis node is the elected exporter for `{}`.",
                here.name
            ))
        );
    }
    // The line that would have saved somebody fifteen minutes. A `·` in the
    // table means the reading lives on another machine, and the table has
    // to say which one — a reader who does not know that reads four zeroes
    // as "healthy and idle" instead of "I am not the one who would know".
    for r in rows
        .iter()
        .filter(|r| r.enabled && !r.status_is_local && !r.elected.is_empty())
    {
        println!(
            "{}",
            term::out().prose(&format!(
                "\n{}: exported from {}. Its lag, spool and last status are files on that \
             machine —\n`·` above means this node has no reading, not that there is nothing \
             wrong. Run\n`meshnode sinks` on {} to see them.",
                r.name, r.elected, r.elected
            ))
        );
    }
    Ok(())
}

/// A lag as an operator reads it. Seconds under a minute, then the largest
/// unit that is still a number.
fn lag_human(secs: u64) -> String {
    match secs {
        0 => "—".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// `meshnode sinks add --dry-run`: the record, before it is signed.
pub fn print_sink_definition(sink: &mesh_proto::Sink) {
    let caps = sink.capabilities();
    println!("{} {}", term::label_w("name", 16), sink.name);
    println!("{} {}", term::label_w("kind", 16), sink.kind.as_str());
    if !sink.url.is_empty() {
        println!("{} {}", term::label_w("url", 16), sink.url);
    }
    if !sink.database.is_empty() {
        println!("{} {}", term::label_w("plugin", 16), sink.database);
    }
    println!(
        "{} {}",
        term::label_w("credential", 16),
        if sink.credential.is_empty() {
            "(none)"
        } else {
            // The reference, never a value — there is no value here to
            // print, which is the point of C6.
            &sink.credential
        }
    );
    println!(
        "{} {}",
        term::label_w("tables", 16),
        sink.tables.names().join(",")
    );
    println!(
        "{} {}",
        term::label_w("run on", 16),
        if sink.run_on.is_empty() {
            "any live executor"
        } else {
            &sink.run_on
        }
    );
    println!(
        "{} {} rows every {}s",
        term::label_w("batching", 16),
        sink.records.batch,
        sink.records.every_secs
    );
    println!(
        "{} {} then drop the oldest",
        term::label_w("spool", 16),
        bytes_human(sink.on_failure.spool_bytes)
    );
    if sink.on_failure.hold_for_export_days > 0 {
        println!(
            "{} {} days, then delete anyway and report the gap",
            term::label_w("hold deletion", 16),
            sink.on_failure.hold_for_export_days
        );
    }
    println!("{} {}", term::label_w("enabled", 16), sink.enabled);

    if sink.payloads.enabled {
        println!(
            "{} yes, up to {}",
            term::label_w("job output", 16),
            if sink.payloads.max_bytes == 0 {
                "any size".to_string()
            } else {
                bytes_human(sink.payloads.max_bytes)
            }
        );
    }

    // §14.4: say out loud what a person can get back out of this, so a
    // read-heavy need does not get pointed at a write-only destination.
    println!();
    println!(
        "{} archives {} · browse records {} · replay stdout {}",
        term::label_w("you can", 16),
        yes_no(caps.archives),
        yes_no(caps.browse_records),
        yes_no(caps.replay_payloads)
    );
    // The sentence, rendered from the triple rather than written beside it,
    // so it cannot drift from what the code does.
    println!("\n{}", sink.kind.capability_sentence());
    if !caps.replay_payloads {
        println!(
            "\nHistory is always read from this mesh's own tiers, never through a sink.\n\
             This destination stores a copy; it does not answer questions."
        );
    }
}

/// The destination list, with what each one gets you — shown **at
/// selection** (M31 B2 item 8).
///
/// A picker that stays silent has already misled the person choosing, and
/// B2 is where the first customer chooses. Rendered from the capability
/// triple on the enum, so a destination added later cannot ship without a
/// sentence.
pub fn print_sink_kinds() {
    println!("Destinations, and what you get back from each:\n");
    let width = mesh_proto::SinkKind::ALL
        .iter()
        .map(|k| k.as_str().len())
        .max()
        .unwrap_or(12);
    for kind in mesh_proto::SinkKind::ALL {
        println!(
            "  {:<width$}  {}",
            kind.as_str(),
            kind.capability_sentence()
        );
        if kind.payloads_by_default() {
            let pad = " ".repeat(width);
            println!("  {pad}  Job output is exported by default here — it is a log store.");
        }
    }
    println!(
        "{}",
        term::out().prose(
            "\nHistory is always read from this mesh's own tiers, never through a sink.\n\
         Start with `ndjson` and point Vector at the file if you are unsure."
        )
    );
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

/// `meshnode retention show` — the windows, and where they leave the log
/// (M31 A1).
///
/// The projection is the point. A knob whose effect nobody can predict is a
/// knob nobody turns, so the size the log settles at is printed beside the
/// windows rather than left for the operator to work out.
pub fn print_retention(view: &mesh_proto::RetentionView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(view)?);
        return Ok(());
    }

    let width = view
        .windows
        .iter()
        .map(|w| w.table.chars().count())
        .max()
        .unwrap_or(10)
        .max(5);
    println!(
        "{:<width$}  {:>8}  {:>8}  {:>9}",
        "TABLE", "MESH", "FLOOR", "HERE"
    );
    for row in &view.windows {
        println!(
            "{:<width$}  {:>8}  {:>8}  {:>9}{}",
            row.table,
            row.window,
            row.floor.as_deref().unwrap_or("—"),
            row.effective_here,
            if row.bounds_the_op_log {
                "   ← bounds the replicated log"
            } else {
                ""
            }
        );
    }

    println!();
    match &view.signed_by {
        Some(who) => println!(
            "{} {who}{}",
            term::label_w("signed by", 22),
            if view.signed_at_micros > 0 {
                format!(" on {}", stamp(view.signed_at_micros))
            } else {
                String::new()
            }
        ),
        // Not a warning: the shipped defaults are finite, so a mesh nobody
        // has configured is already bounded. Saying which of the two you
        // are looking at is the whole of what this line is for.
        None => println!(
            "{} the shipped defaults (nobody has signed a policy)",
            term::label_w("policy", 22)
        ),
    }

    print_retention_projection(view);

    // `HERE` differing from `MESH` is the Q8 rule visible, and it is worth
    // one sentence rather than leaving somebody to wonder which wins.
    if view.windows.iter().any(|w| w.effective_here != w.window) {
        println!(
            "\nThis node keeps less than the mesh window on some tables — its own \
             configuration\nmay shorten a window and never extend one."
        );
    }
    Ok(())
}

/// The two-line answer to *"what does this cost me"*.
fn print_retention_projection(view: &mesh_proto::RetentionView) {
    if view.oplog_bytes == 0 && view.permanent_bytes_per_day == 0 {
        // No census yet — the node has just started. Say so rather than
        // printing zeroes that read as "nothing is growing".
        println!(
            "{} not measured yet (try again in a few minutes)",
            term::label_w("op-log", 22)
        );
        return;
    }
    println!(
        "{} {}",
        term::label_w("op-log now", 22),
        bytes_human(view.oplog_bytes)
    );
    if view.retirable_bytes_per_day > 0 {
        println!(
            "{} ~{}/day",
            term::label_w("closed records", 22),
            bytes_human(view.retirable_bytes_per_day)
        );
    }
    if view.permanent_bytes_per_day > 0 {
        println!(
            "{} ~{}/day",
            term::label_w("permanent (audit, …)", 22),
            bytes_human(view.permanent_bytes_per_day)
        );
    }
    match view.settles_at_bytes {
        Some(bytes) => println!(
            "{} ~{} after a year",
            term::label_w("settles at", 22),
            bytes_human(bytes)
        ),
        // The honest answer, and the reason `0 = forever` is something an
        // operator has to type.
        None => println!(
            "{}",
            term::out().prose(&format!(
                "{:<22} it does not — `runs` is forever",
                "settles at"
            ))
        ),
    }
}

/// `meshnode retention set --dry-run`: what would change, and what it would
/// cost, before anything is signed (M31 A1 item 5).
pub fn print_retention_preview(
    live: &mesh_proto::RetentionView,
    proposed: &mesh_proto::RetentionPolicy,
    json: bool,
) -> Result<()> {
    use mesh_proto::retention::describe_days;

    if json {
        println!("{}", serde_json::to_string_pretty(proposed)?);
        return Ok(());
    }

    println!(
        "{}",
        term::out().dim(format!(
            "{:<12}  {:>10}  {:>10}",
            "TABLE", "NOW", "PROPOSED"
        ))
    );
    for table in mesh_proto::RetentionTable::ALL {
        let before = live.policy.window(table);
        let after = proposed.window(table);
        let changed = before != after;
        println!(
            "{}  {:>10}  {:>10}{}",
            term::out().bold(format!("{:<12}", table.as_str())),
            describe_days(before.days),
            describe_days(after.days),
            if changed { "   ←" } else { "" }
        );
    }

    // Re-project against the proposed `runs` window, from the same
    // measurement the live view carries. This is the number that turns the
    // knob into a decision, and it has to be shown *before* signing.
    let runs = proposed.window(mesh_proto::RetentionTable::Runs);
    println!();
    match runs.is_forever() {
        true => println!(
            "{}",
            term::out().prose("With these settings the log does not settle — `runs` is forever.")
        ),
        false => {
            let settled = live
                .retirable_bytes_per_day
                .saturating_mul(u64::from(runs.days))
                .saturating_add(live.permanent_bytes_per_day.saturating_mul(365));
            if live.permanent_bytes_per_day == 0 && live.retirable_bytes_per_day == 0 {
                println!(
                    "{}",
                    term::out().prose(
                        "This node has not measured its op-log yet, so there is no projection to \
                     show.\nRun `meshnode state census` in a few minutes."
                    )
                );
            } else {
                println!(
                    "With these settings the log settles at ~{} after a year (it is {} now).",
                    bytes_human(settled),
                    bytes_human(live.oplog_bytes)
                );
            }
        }
    }
    Ok(())
}

/// `meshnode state census` — what the replicated log is made of (M31 P0).
///
/// Two columns carry the argument. `ops` is what the log costs today and
/// `permanent` is what it costs for ever, and the gap between them is C0
/// working. A single total would hide the fact that compaction is doing its
/// job while the log still grows without bound, which is exactly the
/// misreading M31 track A exists to correct.
/// What this node is refusing, and what is churning (M31 A3).
///
/// **The empty case is the interesting one, and it says so.** A control
/// that prints a table of zeros trains people to skip it; one that says
/// "nothing is being refused" in a sentence is read.
pub fn print_backpressure(view: &mesh_proto::BackpressureView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(view)?);
        return Ok(());
    }
    println!(
        "limit: {} ops/minute per entity, refused at this node",
        view.limit_per_minute
    );
    if view.entities.is_empty() && view.flapping.is_empty() {
        println!("nothing is being refused or held back.");
        return Ok(());
    }
    if !view.entities.is_empty() {
        println!();
        let width = view
            .entities
            .iter()
            .map(|e| e.entity.chars().count())
            .max()
            .unwrap_or(6)
            .max(6);
        println!(
            "{:<width$}  {:>9}  {:>9}  STATE",
            "ENTITY", "OPS/MIN", "REFUSED"
        );
        for row in &view.entities {
            println!(
                "{:<width$}  {:>9}  {:>9}  {}",
                row.entity,
                row.observed_per_minute,
                row.refused_total,
                if row.paused_until_micros > 0 {
                    // The pause expires by itself; saying so is the whole
                    // difference between a brake and a tombstone.
                    format!(
                        "paused until {} (expires by itself)",
                        mesh_proto::availability::day_of(row.paused_until_micros)
                    )
                } else {
                    "over the limit".to_string()
                }
            );
        }
        println!();
        println!(
            "{}",
            term::out()
                .prose("`meshnode audit --action state.backpressure` has one entry per episode.")
        );
    }
    if !view.flapping.is_empty() {
        println!();
        println!("fact collectors held back for changing again inside the publish window:");
        for key in &view.flapping {
            println!("  {key}");
        }
    }
    Ok(())
}

pub fn print_op_census(census: &mesh_proto::OpCensus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(census)?);
        return Ok(());
    }

    println!(
        "{} {}",
        term::label_w("ops in the log", 22),
        census.ops_total
    );
    println!(
        "{} {}",
        term::label_w("permanent", 22),
        census.surviving_total
    );
    println!(
        "{} {}",
        term::label_w("envelope bytes", 22),
        bytes_human(census.bytes_total)
    );
    println!(
        "{} {}",
        term::label_w("on disk", 22),
        bytes_human(census.disk_bytes)
    );
    if census.undecodable > 0 {
        // F0: carried and forwarded, never classified. Counted rather than
        // omitted, so the total still matches the disk.
        println!(
            "{} {} (from a newer build than this one)",
            term::label_w("not decodable here", 22),
            census.undecodable
        );
    }

    let per_day = census.surviving_bytes_per_day();
    println!();
    println!(
        "{} {}",
        term::label_w("written last day", 22),
        census.ops_last_day
    );
    println!(
        "{} {}",
        term::label_w("of those, permanent", 22),
        census.surviving_last_day
    );
    if per_day > 0 {
        println!(
            "{} ~{}",
            term::label_w("permanent growth/day", 22),
            bytes_human(per_day)
        );
        println!(
            "{} ~{}",
            term::label_w("…which is per year", 22),
            bytes_human(per_day.saturating_mul(365))
        );
    }

    if census.by_kind.is_empty() {
        return Ok(());
    }
    println!();
    let width = census
        .by_kind
        .iter()
        .map(|k| k.kind.chars().count())
        .max()
        .unwrap_or(4)
        .max(6);
    println!(
        "{:<width$}  {:>9}  {:>9}  {:>9}",
        "ENTITY", "OPS", "PERMANENT", "BYTES"
    );
    for row in &census.by_kind {
        println!(
            "{:<width$}  {:>9}  {:>9}  {:>9}",
            row.kind,
            row.ops,
            row.surviving,
            bytes_human(row.bytes)
        );
    }

    // The one line here that is an instruction rather than a reading. Every
    // node holds all of it, so this number is multiplied by the fleet.
    if per_day > 0 {
        println!(
            "\nEvery node in the mesh stores all of this. At this rate a year costs \
             ~{} per node.",
            bytes_human(per_day.saturating_mul(365))
        );
    }
    Ok(())
}

pub fn print_state(stats: &mesh_proto::StateStats, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(stats)?);
        return Ok(());
    }

    if stats.dry_run {
        println!("Dry run — nothing was dropped.\n");
    } else if stats.checkpoint_seq > 0 {
        println!(
            "Compacted. {} ops dropped; checkpoint authored at seq {}.\n",
            stats.dropped, stats.checkpoint_seq
        );
    }

    println!(
        "{} {}",
        term::label_w("ops in the log", 22),
        stats.ops_total
    );
    println!("{} {}", term::label_w("authored here", 22), stats.ops_mine);
    println!(
        "{} {}",
        term::label_w("on disk", 22),
        bytes_human(stats.bytes_on_disk)
    );
    println!(
        "{} {}",
        term::label_w("horizon", 22),
        horizon_human(stats.horizon_secs)
    );
    println!();
    println!(
        "{} {}",
        term::label_w("already compacted", 22),
        stats.pruned_mine
    );
    println!(
        "{:<22} {}",
        if stats.dry_run {
            "would drop now"
        } else {
            "droppable now"
        },
        if stats.dry_run {
            stats.dropped
        } else {
            stats.droppable_now
        }
    );
    println!(
        "{} {}",
        term::label_w("held by the horizon", 22),
        stats.held_by_horizon
    );
    if stats.usage_live > 0 || stats.usage_expired > 0 {
        println!();
        println!(
            "{} {}",
            term::label_w("usage windows live", 22),
            stats.usage_live
        );
        if stats.usage_expired > 0 {
            println!(
                "{} {}",
                term::label_w("aged out, not yet freed", 22),
                stats.usage_expired
            );
        }
    }
    if stats.pruned_mesh > stats.pruned_mine {
        println!(
            "{} {}",
            term::label_w("compacted mesh-wide", 22),
            stats.pruned_mesh
        );
    }

    // M31 A1: the compaction horizon governs *bytes*; the retention window
    // governs *what anyone reads*. Two different questions, and an operator
    // looking at one should be told the other exists rather than
    // discovering it when history goes missing.
    println!(
        "{}",
        term::out().prose(
            "\n`meshnode retention show` — how long this mesh keeps closed history, and where\n\
         that leaves the log. The horizon above is a local byte budget; the window there is\n\
         a mesh-wide policy."
        )
    );

    // The only number here that is an instruction rather than a reading.
    if !stats.dry_run && stats.droppable_now > 0 {
        println!(
            "{}",
            term::out().prose(&format!(
                "\n{} superseded ops are past the horizon. `meshnode state compact` drops them.",
                stats.droppable_now
            ))
        );
    } else if stats.held_by_horizon > 0 && stats.droppable_now == 0 {
        println!(
            "\nNothing to drop yet: {} superseded ops are still inside the {} horizon.",
            stats.held_by_horizon,
            horizon_human(stats.horizon_secs)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The queue (C1 P4)
// ---------------------------------------------------------------------------

pub fn print_queue(rows: &[mesh_proto::QueueRow], by_principal: bool, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("Nothing is waiting on this node.");
        return Ok(());
    }

    if by_principal {
        // Grouped in the order the first of each principal's jobs will run,
        // because "behind whom" is the question this view exists for.
        let mut seen: Vec<(String, u32, usize, u32)> = Vec::new();
        for r in rows {
            match seen.iter_mut().find(|(name, _, _, _)| *name == r.username) {
                Some((_, _, count, _)) => *count += 1,
                None => seen.push((r.username.clone(), r.position, 1, r.share_permille)),
            }
        }
        println!(
            "{}",
            term::out().dim(format!(
                "{:<20} {:>8} {:>8}  SHARE",
                "USER", "WAITING", "NEXT AT"
            ))
        );
        for (name, first, count, share) in seen {
            println!(
                "{} {:>8} {:>8}  {:>4}.{}%",
                term::out().bold(format!("{:<20}", truncate(&name, 20))),
                count,
                first,
                share / 10,
                share % 10
            );
        }
    } else {
        println!(
            "{} {:<20} {:<18} {:<8}  SHARE",
            term::label_w("#", 4),
            "USER",
            "JOB",
            "PRIORITY"
        );
        for r in rows {
            println!(
                "{:<4} {:<20} {:<18} {:<8}  {:>4}.{}%",
                r.position,
                truncate(&r.username, 20),
                r.job.short(),
                format!("{:?}", r.priority).to_lowercase(),
                r.share_permille / 10,
                r.share_permille % 10
            );
        }
    }
    println!("\nOrder is priority, then furthest behind their fair share, then arrival.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Gangs (C4 §8.3)
// ---------------------------------------------------------------------------

pub fn print_gangs(rows: &[mesh_proto::GangRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no gangs — nothing is holding an all-or-nothing reservation");
        println!(
            "{}  {}",
            term::label("run one:"),
            term::cmd("meshnode run ./train.sh --gang 8 --cpus 4 --follow")
        );
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<20} {:<14} {:>5} {:>7} {:<8}  STATE",
            "GANG", "COMMAND", "USER", "SIZE", "HELD", "LAYOUT"
        ))
    );
    for r in rows {
        println!(
            "{} {:<20} {:<14} {:>5} {:>7} {:<8}  {}",
            term::out().dim(format!("{:<16}", r.gang.short())),
            truncate(&r.preview, 20),
            truncate(&r.username, 14),
            r.size,
            r.held,
            r.placement.as_str(),
            term::out().state(&r.state)
        );
    }
    // The order *is* the answer: rows are sorted by ticket, so reading down
    // the table is reading the wound-wait order.
    println!("\nMost senior first: priority, then submission time. A senior gang takes");
    println!("reservations from a junior one rather than both retrying forever.");
    Ok(())
}

pub fn print_gang(view: &mesh_proto::GangView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(view)?);
        return Ok(());
    }
    let r = &view.row;
    println!(
        "{}      {}",
        term::label("gang"),
        r.gang.to_display_string()
    );
    println!("{}   {}", term::label("command"), r.preview);
    println!("{}      {}", term::label("user"), r.username);
    println!("{}     {}", term::label("owner"), r.owner_name);
    println!(
        "{}     {} member(s), {} placement, priority {}",
        term::label("shape"),
        r.size,
        r.placement.as_str(),
        r.ticket.priority().as_str()
    );
    println!(
        "{}     {}",
        term::label("state"),
        term::out().state(&r.state)
    );
    println!();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<5} {:<18} {:<16} {:<22} STATE",
            "RANK", "JOB", "NODE", "ADDRESS"
        ))
    );
    for m in &view.members {
        println!(
            "{} {:<18} {:<16} {:<22} {}",
            term::out().dim(format!("{:<5}", m.rank)),
            m.job_id.short(),
            truncate(&m.node_name, 16),
            if m.addr.is_empty() {
                "-".to_string()
            } else {
                truncate(&m.addr, 22)
            },
            term::out().state(m.state.map_or("-", |s| s.as_str()))
        );
    }
    if !view.ahead.is_empty() {
        println!();
        println!("Ahead of it ({}):", view.ahead.len());
        for g in &view.ahead {
            println!("    {}", g.short());
        }
        // The honest reading of an empty list matters as much as a full
        // one, so the full one says what to do about it.
        println!("These are senior and unfinished. This gang runs when they release.");
    } else if r.state == "offering" {
        println!();
        println!("Nothing is senior to it — it is waiting for room, not for a queue.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deployments (C2 §6)
// ---------------------------------------------------------------------------

pub fn print_deployments(rows: &[mesh_proto::DeploymentRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no deployments — nothing is being kept up");
        println!(
            "{}  {}",
            term::label("write one:"),
            term::cmd("meshnode deployments put edge.toml")
        );
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<18} {:<16} {:<12} {:<20} {:<7} STATUS",
            "NAME", "SERVICE", "COUNT", "TARGETS", "GEN"
        ))
    );
    for r in rows {
        // running/healthy against desired, so the one number that says
        // "is this converged" is on every row.
        let health = format!("{}/{}/{}", r.healthy, r.running, r.desired);
        println!(
            "{} {:<16} {:<12} {:<20} {:<7} {} {}",
            term::out().bold(format!("{:<18}", truncate(&r.name, 17))),
            truncate(&r.service, 15),
            truncate(&r.count, 11),
            truncate(&r.targets, 19),
            r.generation,
            term::out().state(format!("{:<12}", r.status)),
            health
        );
    }
    println!();
    println!("STATUS shows: healthy/running/desired");
    Ok(())
}

/// `meshnode deployment placement` (Phase C, U8): each machine, how much of
/// it its instances asked for, and which instances those are.
///
/// Machines running something come first, fullest first, because "why is
/// everything on one box" is the question that brings somebody here.
pub fn print_placement(rows: &[mesh_proto::PlacementNodeRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no machines");
        return Ok(());
    }
    let mb = |v: u64| {
        if v >= 1024 {
            format!("{:.1} GB", v as f64 / 1024.0)
        } else {
            format!("{v} MB")
        }
    };
    let asked_cpus =
        |r: &mesh_proto::PlacementNodeRow| r.instances.iter().map(|i| i.cpus).sum::<u32>();
    let mut ordered: Vec<&mesh_proto::PlacementNodeRow> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        let share = |r: &mesh_proto::PlacementNodeRow| {
            if r.cores > 0 {
                f64::from(asked_cpus(r)) / f64::from(r.cores)
            } else {
                r.instances.len() as f64
            }
        };
        share(b)
            .total_cmp(&share(a))
            .then(b.instances.len().cmp(&a.instances.len()))
            .then(a.name.cmp(&b.name))
    });
    let now_secs = mesh_proto::now_micros() / 1_000_000;
    for r in ordered {
        let cpus = asked_cpus(r);
        let mem: u64 = r.instances.iter().map(|i| i.mem_mb).sum();
        let said = if r.draining {
            if r.leaving {
                "  shutting down".to_string()
            } else {
                match r.drain_deadline_secs {
                    None => "  draining, no deadline".to_string(),
                    Some(at) if at <= now_secs => "  draining, deadline passed".to_string(),
                    Some(at) => format!("  draining, {}m left", (at - now_secs).div_ceil(60)),
                }
            }
        } else if !r.eligible {
            "  takes no new work".to_string()
        } else if r.liveness != "alive" {
            format!("  {}", r.liveness)
        } else {
            String::new()
        };
        let of = |asked: String, has: String, known: bool| {
            if known {
                format!("{asked} of {has}")
            } else {
                asked
            }
        };
        println!(
            "{}  cores {}  memory {}{}",
            r.name,
            of(cpus.to_string(), r.cores.to_string(), r.cores > 0),
            of(mb(mem), mb(r.mem_mb), r.mem_mb > 0),
            said
        );
        if r.instances.is_empty() {
            println!("    (nothing deployed here)");
        }
        for i in &r.instances {
            let request = match (i.cpus, i.mem_mb) {
                (0, 0) => "no request".to_string(),
                (c, 0) => format!("{c} cores"),
                (0, m) => mb(m),
                (c, m) => format!("{c} cores, {}", mb(m)),
            };
            println!(
                "    {:<28} {:<9} {:<10} {}",
                truncate(&format!("{}/{}", i.deployment, i.slot), 27),
                i.state,
                i.health,
                request
            );
        }
    }
    println!();
    println!("cores and memory are what instances asked for, against what each machine advertises");
    Ok(())
}

pub fn print_deployment(r: &mesh_proto::DeploymentRow, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!("{}  {}", term::label("deployment:"), r.name);
    println!("{}     {}", term::label("service:"), r.service);
    println!("{}       {}", term::label("count:"), r.count);
    println!("{}     {}", term::label("targets:"), r.targets);
    println!("{}  {}", term::label("generation:"), r.generation);
    println!(
        "{}      {}",
        term::label("status:"),
        term::out().state(&r.status)
    );
    println!(
        "{}   {} healthy / {} running / {} desired",
        term::label("instances:"),
        r.healthy,
        r.running,
        r.desired
    );
    println!(
        "{}     {} by {}",
        term::label("created:"),
        stamp(r.created_at_micros),
        r.created_by_username
    );
    Ok(())
}

pub fn print_deployment_status(v: &mesh_proto::DeploymentStatusView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    println!("{}  {}", term::label("deployment:"), v.name);
    println!("{}     {}", term::label("service:"), v.service);
    println!(
        "{}       {}   targets: {}",
        term::label("count:"),
        v.count,
        v.targets
    );
    println!(
        "{}      {}   generation: {}",
        term::label("status:"),
        term::out().state(&v.status),
        v.generation
    );
    println!(
        "{}   {} healthy / {} running / {} desired",
        term::label("instances:"),
        v.healthy,
        v.running,
        v.desired
    );
    println!(
        "{}  {}",
        term::label("reconciler:"),
        v.reconciler
            .clone()
            .unwrap_or_else(|| "- (no live node is eligible; nothing is watching)".to_string())
    );
    // Phase A, U6: what happens when a node goes away, said on every
    // status rather than left to be discovered by losing one.
    println!("{}    {}", term::label("failover:"), v.availability);
    if v.unreachable > 0 {
        // Phase A, U4. Their health column is the last thing their node
        // said, which is exactly the fact that has stopped being true.
        println!(
            "UNREACHABLE: {} instance{} on a node that is not answering — see failover above",
            v.unreachable,
            if v.unreachable == 1 { "" } else { "s" }
        );
    }
    if let Some(from) = v.updating_from {
        // max_unavailable is not carried in the view; the batch is whatever
        // is mid-roll right now, which the instance list below shows.
        println!(
            "{}    rolling from generation {from} to {} — in batches",
            term::label("updating:"),
            v.generation
        );
    }
    if v.awaiting_promotion {
        println!("CANARY:      settled, and holding for a person (auto_promote = false)");
        println!(
            "             promote before the progress deadline stalls it:  meshnode deployment promote {}",
            v.name
        );
    } else if v.canarying {
        println!("CANARY:      new instances are up beside the old ones, still proving themselves");
        println!("             nothing is cut over until every canary has settled");
    }
    if let Some(reason) = &v.degraded_reason {
        println!("DEGRADED:    {reason}");
        // **Not "restarts are stopped" (N7).** They are not: a slot past
        // its budget backs off and is retried on a node it has not already
        // failed on. Saying otherwise sent people to re-publish a
        // deployment that was already recovering on its own.
        println!(
            "             {}",
            concat!(
                "the mesh keeps trying, slower and elsewhere. Fix the cause and it ",
                "clears itself once the window passes — nothing to re-publish."
            )
        );
    }
    if let Some(reason) = &v.stalled_reason {
        println!("STALLED:     {reason}");
        println!("             nothing has crashed — the new version is simply not going healthy");
    }
    if !v.policy_blind.is_empty() {
        // The door-left-open failure, named by machine. Invisible from the
        // roll itself: a roll without a canary looks exactly like a roll.
        println!(
            "POLICY BLIND: {} — too old to apply this deployment's canary, progress deadline",
            v.policy_blind.join(", ")
        );
        println!(
            "             or lost_after. If one is elected to reconcile, it rolls the old way."
        );
    }
    if let Some(refusal) = &v.placement {
        println!();
        print!(
            "{}",
            render_refusal(refusal, v.desired.saturating_sub(v.running), v.desired)
        );
    }
    println!();
    if v.instances.is_empty() {
        println!("no instances yet");
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<12} {:<16} {:<5} {:<11} {:<10} JOB",
            "SLOT", "NODE", "GEN", "STATE", "HEALTH"
        ))
    );
    for i in &v.instances {
        println!(
            "{} {:<16} {:<5} {} {} {}",
            term::out().bold(format!("{:<12}", truncate(&i.slot, 11))),
            truncate(&i.node, 15),
            i.generation,
            term::out().state(format!("{:<11}", i.state)),
            term::out().state(format!("{:<10}", i.health)),
            i.job_id.short()
        );
        // A copy waiting out its backoff: how it ended and when it is tried
        // again, which the row's columns have no room for.
        if matches!(i.state.as_str(), "failed" | "timed-out") && !i.health_detail.is_empty() {
            println!(
                "{}",
                term::out().dim(format!(
                    "             {} — `meshnode deployment logs {} --slot {}`",
                    i.health_detail, v.name, i.slot
                ))
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// N1 P1 — the rollout, watched
// ---------------------------------------------------------------------------

/// `used / limit` in MiB, or just `used` when there is no ceiling.
fn memory_words(used: Option<u64>, limit: Option<u64>) -> String {
    let mib = |bytes: u64| format!("{:.0}MiB", bytes as f64 / 1_048_576.0);
    match (used, limit) {
        (Some(u), Some(l)) => format!("{} / {}", mib(u), mib(l)),
        (Some(u), None) => mib(u),
        _ => "-".into(),
    }
}

/// `deployment status --stats` (Phase B, E3): what each current instance is
/// using, measured on its own node just now.
pub fn print_instance_stats(rows: &[mesh_proto::InstanceStatRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    println!();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<12} {:<16} {:>7}  {:<21} SOURCE",
            "SLOT", "NODE", "CPU", "MEMORY"
        ))
    );
    for r in rows {
        let (cpu, mem, source) = match &r.stat {
            Some(s) => (
                s.cpu_pct
                    .map(|c| format!("{c:.1}%"))
                    .unwrap_or_else(|| "-".into()),
                memory_words(s.mem_bytes, s.mem_limit_bytes),
                if s.source.is_empty() {
                    "unmeasured".to_string()
                } else {
                    s.source.clone()
                },
            ),
            None => ("-".into(), "-".into(), "its node was not asked".into()),
        };
        println!(
            "{} {:<16} {:>7}  {:<21} {source}",
            term::out().bold(format!("{:<12}", truncate(&r.slot, 11))),
            truncate(&r.node, 15),
            cpu,
            mem
        );
    }
    if rows
        .iter()
        .any(|r| r.stat.as_ref().is_some_and(|s| s.source == "process"))
    {
        println!();
        println!("{}", term::out().prose("`process` measures an instance's main process only; anything it forked is not counted."));
    }
    Ok(())
}

/// `deployment instance` (Phase B, E3 and U1): one slot's timeline, its
/// health now, and what it is using.
/// `deployment history` (Phase B, E1): newest first, what changed in each,
/// and how to go back to one.
pub fn print_deployment_history(
    h: &mesh_proto::DeploymentHistoryView,
    diff: bool,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(h)?);
        return Ok(());
    }
    let now = mesh_proto::now_micros();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<6} {:<22} {:<7} {:<9} {:<12} {:<16} CHANGED",
            "GEN", "SERVICE", "COUNT", "PUBLISHED", "BY", "TAGS"
        ))
    );
    for v in &h.versions {
        let dep = &v.version.deployment;
        let count = match dep.count {
            mesh_proto::Count::System => "system".to_string(),
            mesh_proto::Count::Fixed(n) => n.to_string(),
        };
        let mut tags = v.tags.join(",");
        if v.kept_by_tag {
            tags.push_str(" (kept)");
        }
        let changed = if diff || v.changes.is_empty() {
            String::new()
        } else {
            let fields: Vec<&str> = v.changes.iter().map(|c| c.field.as_str()).collect();
            truncate(&fields.join(", "), 48).to_string()
        };
        println!(
            "{} {:<22} {:<7} {:<9} {:<12} {:<16} {}",
            term::out().dim(format!(
                "{:<6}",
                format!("{}{}", v.generation, if v.current { "*" } else { "" })
            )),
            truncate(&dep.service_ref(), 21),
            count,
            micros_ago(now, v.published_at_micros),
            truncate(&v.published_by_username, 11),
            truncate(if tags.is_empty() { "-" } else { tags.as_str() }, 15),
            changed
        );
        if diff {
            for c in &v.changes {
                let side = |s: &str| {
                    if s.is_empty() {
                        "(unset)".to_string()
                    } else {
                        s.to_string()
                    }
                };
                println!(
                    "       {}: {} → {}  ({})",
                    c.field,
                    side(&c.from),
                    side(&c.to),
                    c.effect
                );
            }
        }
    }
    println!();
    println!(
        "* runs now. Kept: the last {} versions, and any a tag names.",
        mesh_proto::DEPLOYMENT_HISTORY_KEEP
    );
    if let Some(older) = h.versions.iter().find(|v| !v.current) {
        println!(
            "{}  meshnode deployment rollback {} --generation {}",
            term::label("go back:"),
            h.deployment,
            older.generation
        );
    }
    Ok(())
}

pub fn print_deployment_instance(
    d: &mesh_proto::DeploymentInstanceDetail,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(d)?);
        return Ok(());
    }
    println!(
        "{}    {} · slot {}",
        term::label("instance:"),
        d.deployment,
        d.slot
    );
    println!(
        "{}     {}",
        term::label("runtime:"),
        if d.runtime.is_empty() {
            "unknown"
        } else {
            &d.runtime
        }
    );
    match &d.current {
        Some(c) => {
            println!(
                "{}         {} on {} · {}{}",
                term::label("now:"),
                c.state,
                c.node,
                c.health,
                if c.health_detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", c.health_detail)
                }
            );
            println!("{}         {}", term::label("job:"), c.job_id.short());
        }
        None => println!(
            "{}         nothing is running in this slot",
            term::label("now:")
        ),
    }
    if let Some(s) = &d.stats {
        println!(
            "{}       {} CPU · {} memory · measured from {}",
            term::label("using:"),
            s.cpu_pct
                .map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "-".into()),
            memory_words(s.mem_bytes, s.mem_limit_bytes),
            if s.source.is_empty() {
                "nothing it could read"
            } else {
                &s.source
            }
        );
    }
    println!();
    if d.incarnations.is_empty() {
        println!("no incarnations yet");
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<5} {:<4} {:<16} {:<11} {:<6} {:<26} JOB",
            "GEN", "INC", "NODE", "STATE", "EXIT", "STARTED"
        ))
    );
    for i in &d.incarnations {
        println!(
            "{} {:<4} {:<16} {} {:<6} {} {}",
            term::out().dim(format!("{:<5}", i.generation)),
            i.incarnation,
            truncate(&i.node, 15),
            term::out().state(format!("{:<11}", i.state)),
            i.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            term::out().dim(format!(
                "{:<26}",
                i.started_at_micros
                    .map(mesh_core::localtime::describe_instant)
                    .unwrap_or_else(|| "-".into())
            )),
            i.job_id.short()
        );
    }
    Ok(())
}

/// How a rollout ended, as the follower decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutOutcome {
    /// Every desired instance is healthy and nothing is left on a prior
    /// generation.
    Converged,
    /// The reconciler gave up on it — a failure budget spent, most often.
    Degraded,
    /// The **deployment's** own `progress_deadline` expired (N1 P3).
    /// A real failure, found by the reconciler, and what fires
    /// `auto_revert`.
    DeadlineExceeded,
    /// The **watch** gave up (`--watch-timeout`). Not a failure: the
    /// rollout is still going and the reconciler is still on it. Kept
    /// distinct from `DeadlineExceeded` because telling somebody their
    /// rollout failed when it is merely slow is the worse of the two
    /// errors, and they are one word apart.
    StoppedWatching,
    /// Paused mid-roll. Not a failure: it will never converge because it
    /// was told not to, and saying so beats spinning until the deadline.
    Paused,
}

impl RolloutOutcome {
    pub fn ok(&self) -> bool {
        matches!(self, RolloutOutcome::Converged)
    }
}

/// Instances of the *current* generation that a check has failed.
///
/// `healthy` and `running` come off the view already; unhealthy does not,
/// because the view counts what is up rather than what is sick. A rollout
/// needs the sick number on the summary line — it is the difference
/// between "not started yet" and "started and failing", which is the one
/// distinction an operator watching a roll actually wants.
pub fn rollout_unhealthy(v: &mesh_proto::DeploymentStatusView) -> u32 {
    v.instances
        .iter()
        .filter(|i| i.generation == v.generation && i.health == "unhealthy")
        .count() as u32
}

/// A placement refusal, as bullets under a headline (N1 P2).
///
/// `wanted` is how many instances are missing, so the headline reads
/// "could not place 1 of 3" rather than making the reader subtract.
pub fn render_refusal(
    refusal: &mesh_proto::PlacementRefusal,
    missing: u32,
    desired: u32,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "! could not place {missing} of {desired} instances");
    for line in refusal.lines() {
        let _ = writeln!(out, "    · {line}");
    }
    out
}

/// One frame of the live rollout table.
///
/// Returns the frame rather than printing it, so the caller can clear and
/// redraw in one write the way `top --follow` does — a print-per-line
/// redraw flickers, and on a slow pipe it tears.
pub fn render_rollout(
    v: &mesh_proto::DeploymentStatusView,
    elapsed: std::time::Duration,
    deadline: Option<std::time::Duration>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "rolling out {} — {}, generation {}",
        v.name, v.service, v.generation
    );
    if let Some(from) = v.updating_from {
        let _ = writeln!(
            out,
            "  replacing generation {from}; old instances stop as new ones pass health"
        );
    }
    let _ = writeln!(
        out,
        "  reconciler: {}",
        v.reconciler
            .clone()
            .unwrap_or_else(|| "- (no live node is eligible; nothing is watching)".to_string())
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<12} {:<16} {:<5} {:<11} {:<10} JOB",
        "SLOT", "NODE", "GEN", "STATE", "HEALTH"
    );
    for i in &v.instances {
        let _ = writeln!(
            out,
            "{:<12} {:<16} {:<5} {:<11} {:<10} {}",
            truncate(&i.slot, 11),
            truncate(if i.node.is_empty() { "-" } else { &i.node }, 15),
            i.generation,
            i.state,
            i.health,
            if i.state == "absent" {
                "-".to_string()
            } else {
                i.job_id.short()
            }
        );
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "  {} desired · {} placed · {} healthy · {} unhealthy · {}",
        v.desired,
        v.running,
        v.healthy,
        rollout_unhealthy(v),
        rollout_clock(elapsed)
    );
    if let Some(d) = deadline {
        // This is the **watch** clock, not the deployment's own
        // `progress_deadline` — and the line says so, because conflating
        // the two would suggest Ctrl-C had consequences for the rollout.
        // It resets whenever an instance reaches healthy, so what is shown
        // is time left on the *current stall*, not on the rollout.
        let _ = writeln!(
            out,
            "  watching for another {} of no progress, then leaving it to run",
            rollout_clock(d.saturating_sub(elapsed))
        );
    }
    if v.awaiting_promotion {
        let _ = writeln!(
            out,
            "  CANARY: settled, and holding for `meshnode deployment promote {}` until the progress deadline.",
            v.name
        );
    } else if v.canarying {
        let _ = writeln!(
            out,
            "  CANARY: the new instances are up beside the old ones. \
             Nothing is cut over until every one of them has settled."
        );
    }
    if let Some(reason) = &v.degraded_reason {
        let _ = writeln!(out, "  DEGRADED: {reason}");
    }
    if let Some(reason) = &v.stalled_reason {
        let _ = writeln!(out, "  STALLED: {reason}");
    }
    if !v.policy_blind.is_empty() {
        let _ = writeln!(
            out,
            "  POLICY BLIND: {} — too old to apply this rollout policy",
            v.policy_blind.join(", ")
        );
    }
    // The refusal, in the same stream as the progress rather than after
    // it: a rollout that is stuck should say why while somebody is still
    // looking at it (N1 P2).
    if let Some(refusal) = &v.placement {
        out.push('\n');
        out.push_str(&render_refusal(
            refusal,
            v.desired.saturating_sub(v.running),
            v.desired,
        ));
    }
    out
}

/// The closing line, printed once, left in the scrollback.
pub fn rollout_verdict(
    v: &mesh_proto::DeploymentStatusView,
    outcome: RolloutOutcome,
    elapsed: std::time::Duration,
) -> String {
    let took = rollout_clock(elapsed);
    match outcome {
        RolloutOutcome::Converged => format!(
            "✓ rollout complete — {}/{} healthy in {took}",
            v.healthy, v.desired
        ),
        RolloutOutcome::Degraded => {
            let why = v
                .degraded_reason
                .clone()
                .unwrap_or_else(|| "the reconciler stopped restarting it".into());
            format!(
                "! rollout failed — {why}\n  {}/{} healthy after {took}. \
                 The deployment stays published; fix the service and re-publish.",
                v.healthy, v.desired
            )
        }
        RolloutOutcome::DeadlineExceeded => format!(
            "! rollout failed — the deployment's progress deadline expired with {}/{} healthy \
             after {took}\n  \
             Nothing reached healthy for the length of the deadline. \
             `meshnode deployments status {}` for why.",
            v.healthy, v.desired, v.name
        ),
        RolloutOutcome::StoppedWatching => format!(
            "· stopped watching — {}/{} healthy after {took}\n  \
             The rollout has not failed; it is still going, and the elected reconciler is \
             still on it.\n  `meshnode deployments status {}` to pick it back up.",
            v.healthy, v.desired, v.name
        ),
        RolloutOutcome::Paused => format!(
            "· rollout paused — {}/{} healthy after {took}\n  \
             A paused deployment reconciles nothing. `meshnode deployments resume {}`.",
            v.healthy, v.desired, v.name
        ),
    }
}

/// `41s`, `3m12s`, `1h04m` — a clock for a line that redraws once a second.
///
/// Neither `short_duration` above (which renders every rollout under an
/// hour as a single unmoving `3m`) nor `humantime` (which renders 72s as
/// `1m 12s` and 3612s as `1h 12s`, changing the string's width as it
/// counts and so shifting the columns after it) will do for this.
fn rollout_clock(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn print_deployment_check(v: &mesh_proto::DeploymentCheckView) -> Result<()> {
    println!("{}    {}", term::label("deployment:"), v.name);
    println!("{}       {}", term::label("service:"), v.service);
    println!("{}         {}", term::label("count:"), v.count);
    println!("{}       {}", term::label("targets:"), v.targets);
    println!(
        "{}   {} node{}",
        term::label("matches now:"),
        v.matching_nodes,
        if v.matching_nodes == 1 { "" } else { "s" }
    );
    if let Some(h) = &v.health {
        println!("{}  {h}", term::label("health check:"));
    } else {
        println!(
            "{}  none (instances judged live by their job alone)",
            term::label("health check:")
        );
    }
    // Phase A, U6 — including the defaults nobody typed, which is the
    // point: this is the sentence a file without a `disconnect` block never
    // used to get.
    println!("{}      {}", term::label("failover:"), v.availability);
    // Phase B, E5: what publishing changes, against what is published —
    // `nomad job plan`'s diff, with the one distinction a person needs most:
    // whether it restarts anything.
    println!();
    if v.first_publish {
        println!(
            "{}       first publish — nothing by this name is running yet",
            term::label("changes:")
        );
    } else if v.changes.is_empty() {
        println!(
            "{}       none — publishing this changes nothing and rolls nothing",
            term::label("changes:")
        );
    } else {
        println!(
            "{}       {}",
            term::label("changes:"),
            if v.rolls {
                "publishing this ROLLS every instance, in batches"
            } else {
                "no instance restarts"
            }
        );
        let width = v
            .changes
            .iter()
            .map(|c| c.field.chars().count())
            .max()
            .unwrap_or(0)
            .min(32);
        for c in &v.changes {
            let mark = match (c.from.is_empty(), c.to.is_empty()) {
                (true, _) => "+",
                (_, true) => "-",
                _ => "~",
            };
            let from = if c.from.is_empty() { "—" } else { &c.from };
            let to = if c.to.is_empty() { "—" } else { &c.to };
            println!(
                "  {mark} {:<width$}  {from} → {to}   ({})",
                c.field, c.effect
            );
        }
    }
    if let Some(refusal) = &v.placement {
        println!();
        println!(
            "{}",
            term::out().sgr("1;33", "NOTHING CAN TAKE IT RIGHT NOW:")
        );
        for line in refusal.lines() {
            println!("  - {line}");
        }
    }
    if v.issues.is_empty() {
        println!();
        if v.placement.is_some() {
            println!("the definition is valid; the mesh has nowhere to run it yet");
        } else {
            println!("no problems — this deployment is ready to publish");
        }
    } else {
        println!();
        println!("PROBLEMS (this will not be published as-is):");
        for issue in &v.issues {
            println!("  - {issue}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// C7 — the action cache (C0–C9 §11)
// ---------------------------------------------------------------------------

/// `deployment validate` (Phase B, E4): what the file says, row by row, from
/// `mesh_core::deployments::describe` — the rows Director shows beside its
/// editor, so the two cannot word one file differently.
pub fn print_deployment_validate(rows: &[(String, String)], json: bool) -> Result<()> {
    if json {
        let map: serde_json::Map<String, serde_json::Value> = rows
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }
    for (label, value) in rows {
        println!("{:<10} {value}", format!("{label}:"));
    }
    println!();
    println!("{}", term::out().prose("valid — this is what the file says. `deployment put --dry-run` asks the mesh what it thinks."));
    Ok(())
}

/// `run --explain-key`: the key, and every field that produced it.
///
/// Field by field with per-field digests, because the question this output
/// exists to answer is never "what is my key" — it is *"why is this key
/// different from last time"*, and that is a diff of eleven short digests
/// rather than a comparison of one long one.
pub fn print_key_explain(explain: &mesh_proto::KeyExplain, json: bool) -> Result<()> {
    if json {
        // The explanation rides the JSON too (L2 P4). A script that has to
        // re-derive the sentence from the fields is a second place the
        // sentence lives, and the two would eventually disagree.
        println!("{}", with_why(explain, [("why", explain.why())])?);
        return Ok(());
    }
    println!(
        "{}  {}",
        term::label("action"),
        explain.key.to_display_string()
    );
    println!();
    println!(
        "{}",
        term::out().dim(format!("{:<14} {:<9}  VALUE", "FIELD", "DIGEST"))
    );
    for field in &explain.fields {
        println!(
            "{} {:<9}  {}",
            term::out().bold(format!("{:<14}", field.name)),
            field.digest,
            truncate(&field.value, 60)
        );
    }
    println!();
    println!("{}", explain.why());
    Ok(())
}

/// Merge derived sentences into a struct's JSON body (L2 P4).
///
/// The sentences are *computed* — `PlacementExplain::why()` and friends —
/// so they are not fields on the IPC struct and adding them there would
/// make a derived value look like replicated state. This puts them beside
/// the numbers they explain in `--output json` without either surface
/// owning a second copy of the words.
fn with_why<T: serde::Serialize, const N: usize>(
    value: &T,
    extra: [(&str, String); N],
) -> Result<String> {
    let mut body = serde_json::to_value(value)?;
    if let Some(map) = body.as_object_mut() {
        for (key, sentence) in extra {
            map.insert(key.to_string(), serde_json::Value::String(sentence));
        }
    }
    Ok(serde_json::to_string_pretty(&body)?)
}

/// `meshnode identity hardware ls` (Y2/Y3).
pub fn print_agent_keys(
    socket: &std::path::Path,
    keys: &[mesh_core::hardware::HardwareKey],
    json: bool,
) -> Result<()> {
    if json {
        let rows: Vec<serde_json::Value> = keys
            .iter()
            .map(|k| {
                let (origin, file) = origin_words(&k.origin());
                serde_json::json!({
                    "user_id": k.user_id().to_display_string(),
                    "comment": k.comment(),
                    "agent": socket.display().to_string(),
                    "origin": origin,
                    "file": file,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if keys.is_empty() {
        println!(
            "the agent at {} is holding no Ed25519 keys",
            socket.display()
        );
        println!();
        println!("  For an OpenPGP card (firmware 5.2.3+):");
        println!("    export SSH_AUTH_SOCK=$(gpgconf --list-dirs agent-ssh-socket)");
        println!("  For an Ed25519 key in PIV slot 9a (firmware 5.7+):");
        println!("    ssh-add -s /usr/lib64/opensc-pkcs11.so   (or libykcs11.so)");
        return Ok(());
    }
    println!("{}  {}", term::label("agent"), socket.display());
    println!();
    println!(
        "{}",
        term::out().dim(format!("{:<56} {:<22} KEY", "IDENTITY", "WHERE"))
    );
    for k in keys {
        let origin = k.origin();
        let place = match &origin {
            mesh_core::hardware::KeyOrigin::File(f) => format!("file {}", tilde(f)),
            mesh_core::hardware::KeyOrigin::Device => "security key".into(),
            mesh_core::hardware::KeyOrigin::Unknown => "not a file here".into(),
        };
        println!(
            "{} {:<22} {}",
            term::out().dim(format!("{:<56}", k.user_id().to_display_string())),
            place,
            k.comment()
        );
    }
    let files = keys
        .iter()
        .filter(|k| matches!(k.origin(), mesh_core::hardware::KeyOrigin::File(_)))
        .count();
    println!();
    if files > 0 {
        println!("A `file` key is a key file on this computer — binding to it protects nothing.");
    }
    if files < keys.len() {
        println!(
            "{}",
            term::out().prose(
                "A key that is not a file here is likely on a device, not proven to be: \
                 `identity hardware attest` is what makes the mesh able to tell."
            )
        );
    }
    Ok(())
}

/// `origin` and `file` for a JSON row.
fn origin_words(origin: &mesh_core::hardware::KeyOrigin) -> (&'static str, Option<String>) {
    match origin {
        mesh_core::hardware::KeyOrigin::File(f) => ("file", Some(f.display().to_string())),
        mesh_core::hardware::KeyOrigin::Device => ("device", None),
        mesh_core::hardware::KeyOrigin::Unknown => ("unknown", None),
    }
}

/// `meshnode identity hardware status` (Y2/Y3).
///
/// Reads the key *file*, never the agent, and then says whether the agent
/// currently holds what the file points at. Those are two different
/// questions and an operator standing at a prompt needs both: one is "what
/// did I configure", the other is "will my next command work".
pub fn print_hardware_status(path: &std::path::Path, json: bool) -> Result<i32> {
    let stored = mesh_core::keyfile::Stored::read(path);
    let (kind, public) = match &stored {
        Ok(mesh_core::keyfile::Stored::Hardware(public)) => ("hardware", Some(*public)),
        Ok(mesh_core::keyfile::Stored::Plain(seed)) => (
            "software",
            Some(mesh_core::identity::Keypair::from_seed(*seed).public_bytes()),
        ),
        Ok(mesh_core::keyfile::Stored::Sealed(_)) => ("software, sealed", None),
        Err(_) => ("absent", None),
    };
    // Only asked when the file says hardware: probing an agent for a
    // software identity would report a stranger's key as if it mattered.
    let held = match (kind, public) {
        ("hardware", Some(public)) => Some(
            mesh_core::hardware::agent_socket(None)
                .and_then(|sock| mesh_core::hardware::find(&sock, &public).map(|k| (sock, k))),
        ),
        _ => None,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path.display().to_string(),
                "kind": kind,
                "identity": public.map(|p| mesh_proto::UserId(p).to_display_string()),
                "agent_holding": held.as_ref().map(|h| h.is_ok()),
                "agent": held.as_ref().and_then(|h| h.as_ref().ok())
                    .map(|(sock, _)| sock.display().to_string()),
                "origin": held.as_ref().and_then(|h| h.as_ref().ok())
                    .map(|(_, key)| origin_words(&key.origin()).0),
                "file": held.as_ref().and_then(|h| h.as_ref().ok())
                    .and_then(|(_, key)| origin_words(&key.origin()).1),
            }))?
        );
        return Ok(match held {
            Some(Err(_)) => 1,
            _ => 0,
        });
    }

    println!("{}    {}", term::label("key file"), path.display());
    let held_words = if kind == "hardware" {
        "by a key agent"
    } else {
        kind
    };
    println!("{}        {held_words}", term::label("held"));
    if let Some(public) = public {
        println!(
            "{}    {}",
            term::label("identity"),
            mesh_proto::UserId(public)
        );
    }
    match held {
        None => {
            if kind == "software" || kind == "software, sealed" {
                println!();
                println!("The private key is on this disk. To move onto a token:");
                println!("  meshnode identity hardware ls     what your agent is holding");
            }
            Ok(0)
        }
        Some(Ok((sock, key))) => {
            println!("{}       {}", term::label("agent"), sock.display());
            if !key.comment().is_empty() {
                println!("{}         {}", term::label("key"), key.comment());
            }
            println!();
            println!("The agent is holding it: signing will work.");
            // Honest about what the agent cannot say: a key that is one of
            // the files in ~/.ssh is on this disk, whatever this file calls
            // it (2026-09-18).
            if let mesh_core::hardware::KeyOrigin::File(file) = key.origin() {
                println!(
                    "{}",
                    term::out().prose(&format!(
                        "It is {}, a key file on this computer — not a key on a security \
                         key, so there is a private key on this disk after all.",
                        tilde(&file)
                    ))
                );
            }
            Ok(0)
        }
        Some(Err(e)) => {
            println!();
            // Not an error about the configuration — the binding is right
            // and the token is simply not here. Exit 1 so a script can tell.
            println!("The agent is NOT holding it, so nothing can be signed right now.");
            println!("  {e}");
            Ok(1)
        }
    }
}

pub fn print_cache_stats(stats: &mesh_proto::CacheStats, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(stats)?);
        return Ok(());
    }
    let rate = |hits: u64, misses: u64| -> String {
        match hits + misses {
            0 => "—".to_string(),
            total => format!("{:.0}%", 100.0 * hits as f64 / total as f64),
        }
    };
    if !stats.enabled {
        println!("the action cache is disabled on this node ([cache] enabled = false)");
        println!();
    }
    println!(
        "{}   {} of {} (epoch {})",
        term::label("entries"),
        stats.entries,
        stats.capacity,
        stats.epoch
    );
    println!(
        "{}      {} · misses {} · rate {}",
        term::label("hits"),
        stats.hits,
        stats.misses,
        rate(stats.hits, stats.misses)
    );
    println!("{}     {}", term::label("saved"), bytes(stats.bytes_saved));
    println!("published {} · evicted {}", stats.published, stats.evicted);
    if stats.verify_rate > 0.0 {
        println!(
            "{}  {} at {:.1}% · {} mismatched",
            term::label("verified"),
            stats.verified,
            stats.verify_rate * 100.0,
            stats.mismatched
        );
    }
    if stats.mismatched > 0 {
        // The only number on this page that is an emergency.
        println!();
        println!(
            "{}",
            crate::term::out().status(&format!(
                "!! {} spot-check(s) disagreed with the entry they were checking.",
                stats.mismatched
            ))
        );
        println!(
            "{}",
            term::out().prose("   `meshnode audit --action cache.mismatch` has the detail.")
        );
    }
    if !stats.by_service.is_empty() {
        println!();
        println!(
            "{}",
            term::out().dim(format!(
                "{:<24} {:>7} {:>8} {:>7} {:>10}",
                "SERVICE", "HITS", "MISSES", "RATE", "SAVED"
            ))
        );
        let mut rows: Vec<&mesh_proto::CacheStatRow> = stats.by_service.iter().collect();
        // Biggest saver first: the interesting row is the one paying for
        // the feature, not the one alphabetically first.
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes_saved));
        for r in rows {
            println!(
                "{} {:>7} {:>8} {:>7} {:>10}",
                term::out().bold(format!("{:<24}", truncate(&r.service, 24))),
                r.hits,
                r.misses,
                rate(r.hits, r.misses),
                bytes(r.bytes_saved)
            );
        }
    }
    if stats.hits == 0 && stats.misses == 0 {
        println!();
        println!(
            "{}",
            term::out()
                .prose("Nothing has asked to be cached yet. Mark a service `cacheable`, or:")
        );
        println!("  meshnode run ./build.sh --cacheable --cache-outputs 'out/*'");
    }
    // This node's shard, not the mesh's — said plainly, because a hit rate
    // that silently meant something else would be worse than no hit rate.
    println!();
    println!("Counted on this node: its directory shard, and the lookups it made.");
    Ok(())
}

// ---------------------------------------------------------------------------
// C8 — legibility (C0–C9 §12)
// ---------------------------------------------------------------------------

/// `jobs explain <id>`: why it landed there, term by term.
///
/// **The CLI gets the placement chart first**, per the milestone rule: no
/// UI work before the engine behaviour exists at the command line. What
/// Director draws as a stacked bar is this table with the bars filled in.
pub fn print_job_explain(view: &mesh_proto::JobExplainView, json: bool) -> Result<()> {
    if json {
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(why) = view.placement.as_ref().and_then(|p| p.why()) {
            extra.push(("why", why));
        }
        println!(
            "{}",
            match extra.pop() {
                Some(one) => with_why(view, [one])?,
                None => serde_json::to_string_pretty(view)?,
            }
        );
        return Ok(());
    }
    println!(
        "{}       {}",
        term::label("job"),
        view.job.to_display_string()
    );
    println!("{}   {}", term::label("command"), view.cmd_preview);
    println!(
        "{}     {}",
        term::label("state"),
        term::out().state(&view.state)
    );
    // The sentence goes *next to the value it explains* (L2 P4), not at the
    // bottom of a table somebody has to reach. Everything below it is the
    // arithmetic behind this line.
    println!("{}    {}", term::label("ran on"), view.executor_name);
    if let Some(why) = view.placement.as_ref().and_then(|p| p.why()) {
        println!("          {why}");
    }
    if let Some(micros) = view.duration_micros {
        println!("{}      {}", term::label("took"), duration_micros(micros));
    }

    if let Some(hit) = &view.cache_hit {
        println!();
        println!(
            "{}",
            crate::term::out().status("⚡ served from the action cache")
        );
        println!(
            "   {}        {}",
            term::label("key"),
            hit.key.to_display_string()
        );
        println!(
            "   {}   {} on {}",
            term::label("produced"),
            hit.produced_by.short(),
            hit.producer.short()
        );
        println!(
            "   {}      {}",
            term::label("saved"),
            bytes(hit.bytes_saved)
        );
    } else if let Some(key) = &view.action_key {
        println!();
        println!("action key {}", key.to_display_string());
        println!("(this run was a miss and published its own result)");
    }

    let Some(placement) = &view.placement else {
        println!();
        match view.placement_on {
            Some(node) => {
                println!("No placement breakdown here. It is job-record data on the node that");
                println!("submitted the job — ask {}:", node.short());
                println!(
                    "  meshnode --node {} jobs explain {}",
                    node.short(),
                    view.job.short()
                );
            }
            None if view.cache_hit.is_some() => {
                // Not a missing record: a cache hit places nothing, which
                // is the whole point of it.
                println!("Nothing was placed — this job's answer came from the cache.");
            }
            None => println!("No placement breakdown was kept for this job."),
        }
        return Ok(());
    };

    println!();
    println!(
        "Scored {} candidate(s); the top {} kept their breakdown. Lower is better.",
        placement.considered,
        placement.candidates.len()
    );
    if placement.spilled {
        println!("The best candidate answered Busy, so this job spilled to the next one.");
    }
    println!();

    // One column per term, one row per candidate: the shape the chart has.
    let names: Vec<String> = placement
        .candidates
        .first()
        .map(|c| c.terms.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();
    print!("{:<18} {:>7}", "NODE", "TOTAL");
    for name in &names {
        print!(" {:>9}", truncate(name, 9));
    }
    println!();
    for candidate in &placement.candidates {
        let mark = if Some(candidate.node) == placement.chosen {
            "→"
        } else {
            " "
        };
        print!(
            "{mark}{:<17} {:>7.3}",
            truncate(&candidate.name, 17),
            candidate.total
        );
        for name in &names {
            match candidate.terms.iter().find(|t| t.name == *name) {
                // A term whose input could not be measured is marked, not
                // hidden: an unmeasured node must not read as a mediocre
                // one (M9 §5.2).
                Some(t) if t.unknown && t.weighted == 0.0 => print!(" {:>9}", "—"),
                Some(t) if t.unknown => print!(" {:>8.3}?", t.weighted),
                Some(t) => print!(" {:>9.3}", t.weighted),
                None => print!(" {:>9}", "—"),
            }
        }
        println!();
    }
    println!();
    println!(
        "{}",
        term::out().prose("Each cell is raw × weight — what that term cost the node. `?` marks an")
    );
    println!("input nothing could measure, which scores neutral rather than badly.");
    // What the column headings mean, once, under the table that uses them
    // (L2 P4). These are the scheduler's own names and stay that way; what
    // was missing was anywhere to find out what they measure short of
    // reading `scheduler.rs`.
    if !names.is_empty() {
        println!();
        for name in &names {
            if let Some(gloss) = mesh_proto::term_gloss(name) {
                println!("  {name:<9} {gloss}");
            }
        }
    }
    Ok(())
}

/// `meshnode plan`: where it would land, when it would finish, what it
/// delays.
pub fn print_plan(plan: &mesh_proto::PlanEstimate, json: bool) -> Result<()> {
    if json {
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(why) = plan.placement.why() {
            extra.push(("why", why));
        }
        if let Some(d) = &plan.duration {
            extra.push(("durationWhy", d.why()));
        }
        println!(
            "{}",
            match extra.len() {
                0 => serde_json::to_string_pretty(plan)?,
                1 => with_why(plan, [extra.remove(0)])?,
                _ => {
                    let second = extra.remove(1);
                    with_why(plan, [extra.remove(0), second])?
                }
            }
        );
        return Ok(());
    }
    if let Some(refusal) = &plan.refusal {
        println!("this would not start: {refusal}");
        return Ok(());
    }
    let chosen = plan
        .placement
        .chosen_score()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "—".into());
    println!("{}   {chosen}", term::label("would land on"));
    if let Some(why) = plan.placement.why() {
        println!("                {why}");
    }
    println!(
        "{}           {} free of {} wanted",
        term::label("slots"),
        plan.slots_free,
        plan.slots_wanted
    );
    if plan.wait_secs > 0 {
        println!(
            "{}           ~{}",
            term::label("waits"),
            short_secs(plan.wait_secs)
        );
    } else {
        println!("{}           starts now", term::label("waits"));
    }
    // A range, never a time (§12.2). A planner that says "17 minutes" is
    // wrong precisely often enough to be untrusted forever.
    match (&plan.duration, &plan.finish) {
        (Some(d), Some(f)) => {
            println!("{}        {}", term::label("runs for"), d.describe());
            println!("{}     {}", term::label("finishes in"), f.describe());
            // The provenance sits under the figures it qualifies, in the
            // same words Director's hover uses (L2 P4).
            println!("                {}", d.provenance());
        }
        _ => {
            println!(
                "{}     unknown — nothing like this has run here before",
                term::label("finishes in")
            );
        }
    }
    if !plan.delays.is_empty() {
        println!();
        println!(
            "{}",
            term::out().dim(format!("{:<20} {:>6}  DELAYED BY", "PRINCIPAL", "JOBS"))
        );
        for d in &plan.delays {
            println!(
                "{} {:>6}  ~{}",
                term::out().bold(format!("{:<20}", truncate(&d.name, 20))),
                d.jobs,
                short_secs(d.by_secs)
            );
        }
        println!();
        println!("These principals are ahead of you on fair share; your work runs in front");
        println!("of theirs and pushes them back by about this much.");
    }
    println!();
    println!(
        "{}",
        term::out()
            .prose("Nothing was submitted. `--dry-run` on `run` answers the other question:")
    );
    println!("what this would touch.");
    Ok(())
}

fn short_secs(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", (secs + 30) / 60)
    } else {
        format!("{:.1}h", secs as f64 / 3600.0)
    }
}

/// L1 P5 — say what happened.
///
/// The copy is the deliverable here, not the arithmetic, so these assert
/// the sentences. The one they exist to defend is the last: a mesh that
/// could not reach two machines must never be reported with the same word
/// as a script that returned 2.
#[cfg(test)]
mod said {
    use super::*;
    use mesh_proto::JobState;

    #[test]
    fn an_exit_code_means_the_command_answered() {
        assert_eq!(job_outcome(JobState::Succeeded, Some(0)), "succeeded");
        assert_eq!(
            job_outcome(JobState::Failed, Some(2)),
            "your command exited 2"
        );
        assert!(!is_no_answer(JobState::Failed, Some(2)));
        assert!(!is_no_answer(JobState::Succeeded, Some(0)));
    }

    #[test]
    fn no_exit_code_means_the_mesh_never_got_one() {
        for (state, exit) in [
            (JobState::Failed, None),
            (JobState::TimedOut, None),
            // A timeout is the mesh giving up even when a code arrives
            // late: what the caller has is still no answer.
            (JobState::TimedOut, Some(0)),
        ] {
            assert!(
                is_no_answer(state, exit),
                "{state:?}/{exit:?} is the mesh's problem, not the command's"
            );
            let said = job_outcome(state, exit);
            assert!(
                said.contains("mesh"),
                "{said:?} must name the mesh, not blame the command"
            );
        }
        // …and the word "failed" appears in neither, because it is the
        // word that used to mean both.
        assert!(!job_outcome(JobState::Failed, None).contains("failed"));
        assert!(!job_outcome(JobState::TimedOut, None).contains("failed"));
    }

    fn said(lines: &[Vec<(Tone, String)>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|(_, s)| s.as_str()).collect())
            .collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_clean_fanout_says_so_in_one_line() {
        let t = FanTally {
            succeeded: 4,
            took: std::time::Duration::from_millis(580),
            ..FanTally::default()
        };
        assert_eq!(
            said(&fanout_outcome_lines(&t)),
            ["✓ All 4 machines succeeded · 580ms"]
        );
    }

    #[test]
    fn two_unreachable_nodes_say_so_in_one_sentence() {
        // L1 P5's exit demo.
        let t = FanTally {
            succeeded: 2,
            no_answer: names(&["APUS", "merope"]),
            took: std::time::Duration::from_secs(30),
            ..FanTally::default()
        };
        let lines = said(&fanout_outcome_lines(&t));
        assert_eq!(lines.len(), 3, "one tally, one sentence: {lines:?}");
        assert_eq!(lines[0], "? 2 of 4 succeeded · 2 never answered · 30s");
        assert!(lines[1].contains("The mesh could not get an answer from APUS and merope"));
        assert!(
            lines[2].contains("unknown"),
            "a caller must not be left assuming the command ran: {lines:?}"
        );
    }

    #[test]
    fn the_two_kinds_of_failure_are_never_merged() {
        let t = FanTally {
            succeeded: 1,
            exited_nonzero: names(&["NIM", "IVY"]),
            no_answer: names(&["APUS"]),
            ..FanTally::default()
        };
        let lines = fanout_outcome_lines(&t);
        let all = said(&lines).join("\n");
        assert!(all.contains("2 exited non-zero"), "{all}");
        assert!(all.contains("1 never answered"), "{all}");
        // Both explanations, and each names whose problem it is — and
        // where.
        assert!(all.contains("Your command failed on NIM and IVY"), "{all}");
        assert!(
            all.contains("The mesh could not get an answer from APUS"),
            "{all}"
        );
        // The two kinds never share a colour either.
        let tone_of = |needle: &str| {
            lines[0]
                .iter()
                .find(|(_, s)| s.contains(needle))
                .map(|(t, _)| *t)
        };
        assert_eq!(tone_of("exited non-zero"), Some(Tone::Bad));
        assert_eq!(tone_of("never answered"), Some(Tone::Unknown));
    }

    #[test]
    fn a_machine_nobody_named_is_counted_not_guessed() {
        // The origin gave up on two targets without a frame for either:
        // there is no name to give, so none is made up.
        let t = FanTally {
            succeeded: 2,
            no_answer: names(&["APUS"]),
            no_answer_unnamed: 2,
            ..FanTally::default()
        };
        let all = said(&fanout_outcome_lines(&t)).join("\n");
        assert!(all.contains("3 never answered"), "{all}");
        assert!(all.contains("an answer from 3 machines"), "{all}");
        assert!(!all.contains("1 machines"), "{all}");
    }

    #[test]
    fn a_halted_fanout_says_what_stopped_it() {
        let t = FanTally {
            succeeded: 1,
            exited_nonzero: names(&["NIM", "IVY"]),
            skipped: 5,
            ..FanTally::default()
        };
        let all = said(&fanout_outcome_lines(&t)).join("\n");
        assert!(all.contains("5 never started"), "{all}");
        assert!(all.contains("--halt-on-fail"), "{all}");
    }

    #[test]
    fn many_names_are_cut_short() {
        assert_eq!(some_names(&names(&["NIM"])), "NIM");
        assert_eq!(some_names(&names(&["NIM", "APUS"])), "NIM and APUS");
        assert_eq!(some_names(&names(&["a", "b", "c"])), "a, b and c");
        assert_eq!(
            some_names(&names(&["a", "b", "c", "d", "e"])),
            "a, b and 3 more"
        );
    }
}

#[cfg(test)]
mod links {
    use super::*;

    /// L2 P5's shape. `director://` and not `https://`: there is no server
    /// in this product, and a link that implied one would be the first
    /// thing the mesh ever said that was not true of it.
    #[test]
    fn a_link_names_the_kind_and_the_object() {
        assert_eq!(
            deep_link(DeepLinkKind::Job, "job-agqgubvgeywnxzaszhsolmv4uu"),
            "director://jobs/job-agqgubvgeywnxzaszhsolmv4uu"
        );
        assert_eq!(
            deep_link(DeepLinkKind::Fanout, "fan-agqgt3msu4"),
            "director://fanouts/fan-agqgt3msu4"
        );
        assert_eq!(
            deep_link(DeepLinkKind::Script, "deploy"),
            "director://scripts/deploy"
        );
        assert_eq!(
            deep_link(DeepLinkKind::Node, "mw1abc"),
            "director://nodes/mw1abc"
        );
    }

    /// Every kind is a route Director actually has. A link to a screen that
    /// does not open on that object is worse than no link, because the
    /// person who pasted it has already promised a colleague it works.
    #[test]
    fn the_kinds_are_the_routes() {
        let kinds = [
            DeepLinkKind::Job,
            DeepLinkKind::Fanout,
            DeepLinkKind::Node,
            DeepLinkKind::Script,
            DeepLinkKind::Deployment,
        ];
        let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            ["jobs", "fanouts", "nodes", "scripts", "deployments"]
        );
    }
}

// ---------------------------------------------------------------------------
// Content store and disk (M31 A2)
// ---------------------------------------------------------------------------

/// What this node is holding, and what it would take back.
///
/// A paragraph rather than a table, because there is one row and the
/// interesting part is the *relationship* between the numbers — held
/// against reclaimable, free against the floor. A table of one row makes an
/// operator do that arithmetic themselves.
pub fn print_content_status(v: &mesh_proto::ContentStatusView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    // Zero days is `forever` for `keep` and `immediately` for `ephemeral`.
    // The two ends of the ladder share a number and mean opposite things,
    // so the policy name is what decides which sentence to print.
    let window = match (v.policy.as_str(), v.window_days) {
        ("ephemeral", _) => "as soon as nothing needs it".to_string(),
        (_, 0) => "forever".to_string(),
        (_, days) => format!("{days}d"),
    };
    println!(
        "  Policy     {} ({window}){}",
        v.policy,
        if v.capped_by_mesh {
            "  — shortened by the mesh retention policy"
        } else {
            ""
        }
    );
    println!(
        "  Holding    {} in {} bundles ({} chunks)",
        human_bytes(v.bytes),
        v.bundles,
        v.chunks
    );
    println!("  Protected  {} bundles something still needs", v.protected);
    if v.reclaimable_bundles == 0 {
        println!("  Reclaim    nothing is past its window");
    } else {
        println!(
            "  Reclaim    {} in {} bundles",
            human_bytes(v.reclaimable_bytes),
            v.reclaimable_bundles
        );
    }
    if v.max_bytes > 0 {
        println!("  Cap        {}", human_bytes(v.max_bytes));
    }
    let pct = if v.disk_total_bytes > 0 {
        format!(
            " ({:.0}%)",
            100.0 * v.disk_free_bytes as f64 / v.disk_total_bytes as f64
        )
    } else {
        String::new()
    };
    println!(
        "  Disk       {} free{pct}, floor {}",
        human_bytes(v.disk_free_bytes),
        human_bytes(v.disk_floor_bytes)
    );
    if v.disk_critical {
        println!();
        println!(
            "{}",
            term::out().prose(
                "This node is under its disk floor: it takes no new jobs or bundles, and\n\
             keeps no more output from the jobs it is running. Free space on the data\n\
             dir, or lower `min_free_mb` in [disk]."
            )
        );
    } else if v.reclaimable_bundles > 0 {
        println!();
        println!(
            "{}",
            term::out()
                .prose("`meshnode content prune` takes it now; the hourly sweep takes it anyway.")
        );
    }
    Ok(())
}

pub fn print_content_prune(v: &mesh_proto::ContentPruneView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    let verb = if v.dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    if v.bundles_removed == 0 && v.orphan_chunks == 0 {
        println!(
            "nothing to reclaim ({} bundles are still needed)",
            v.protected
        );
        return Ok(());
    }
    println!(
        "{verb} {} — {} bundles, {} chunks",
        human_bytes(v.bytes_reclaimed),
        v.bundles_removed,
        v.chunks_removed
    );
    if v.orphan_chunks > 0 {
        println!(
            "{verb} a further {} in {} orphaned chunks",
            human_bytes(v.orphan_bytes),
            v.orphan_chunks
        );
    }
    println!(
        "{} remaining, {} bundles kept because something still needs them",
        human_bytes(v.bytes_remaining),
        v.protected
    );
    Ok(())
}

/// Free space across the mesh, worst first.
pub fn print_disk(rows: &[mesh_proto::DiskRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no nodes");
        return Ok(());
    }
    let mut t = table(&["NODE", "FREE", "TOTAL", "USED", "STATE"], &[1, 2, 3]);
    for r in rows {
        // Unknown free space is not 100% used. The arithmetic would say so
        // — zero free of thirty gigabytes — which is the most alarming
        // possible rendering of "this node has not reported yet".
        let used = if r.state != "unknown" && r.total_bytes > 0 && r.free_bytes <= r.total_bytes {
            format!(
                "{:.0}%",
                100.0 * (1.0 - r.free_bytes as f64 / r.total_bytes as f64)
            )
        } else {
            "—".to_string()
        };
        t.add_row(vec![
            if r.online {
                r.name.clone()
            } else {
                format!("{} (offline)", r.name)
            },
            if r.state == "unknown" {
                "—".to_string()
            } else {
                human_bytes(r.free_bytes)
            },
            if r.total_bytes > 0 {
                human_bytes(r.total_bytes)
            } else {
                "—".to_string()
            },
            used,
            r.state.clone(),
        ]);
    }
    println!("{t}");
    if rows.iter().any(|r| r.state == "low") {
        println!();
        println!(
            "{}",
            term::out().prose(
                "`low` means under this node's [disk] floor: those nodes are reclaiming\n\
             their content stores and will refuse new bundles until there is room."
            )
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// N1 P5 — schedules
// ---------------------------------------------------------------------------

/// `in 4m`, `in 6h`, `9m ago` — an instant relative to now.
///
/// The only number a schedule table is actually read for is *"when next"*,
/// and a wall-clock stamp makes a person do the subtraction in their head
/// against a machine whose clock they have not checked.
fn relative(micros: u64) -> String {
    let now = mesh_proto::now_micros();
    if micros >= now {
        format!("in {}", short_duration(micros - now))
    } else {
        format!("{} ago", short_duration(now.saturating_sub(micros)))
    }
}

pub fn print_schedules(rows: &[mesh_proto::ServiceScheduleRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no schedules");
        println!();
        println!("A schedule runs a service on a clock, with nobody watching:");
        println!("  meshnode schedule put nightly-backup.toml");
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<18} {:<16} {:<16} {:<10} {:<12} NEXT",
            "NAME", "SERVICE", "WHEN", "STATUS", "LAST"
        ))
    );
    for r in rows {
        println!(
            "{} {:<16} {:<16} {} {} {}",
            term::out().bold(format!("{:<18}", truncate(&r.name, 17))),
            truncate(&r.service, 15),
            truncate(&r.when, 15),
            term::out().state(format!("{:<10}", r.status)),
            term::out().dim(format!(
                "{:<12}",
                r.last_micros.map(relative).unwrap_or_else(|| "-".into())
            )),
            match (&r.status[..], r.next_micros) {
                // A paused schedule has a next occurrence arithmetically
                // and will not fire at it, and printing one would be the
                // table's only lie.
                ("paused", _) => "-".to_string(),
                (_, Some(next)) => relative(next),
                (_, None) => "never".to_string(),
            }
        );
    }
    if rows.iter().any(|r| r.status == "orphaned") {
        println!();
        println!("ORPHANED: the service it names is gone. It will not fire until that is fixed.");
    }
    Ok(())
}

pub fn print_schedule(r: &mesh_proto::ServiceScheduleRow) -> Result<()> {
    println!("{}    {}", term::label("schedule:"), r.name);
    println!("{}     {}", term::label("service:"), r.service);
    println!("{}        {}", term::label("when:"), r.when);
    println!("{}     {}", term::label("targets:"), r.targets);
    println!(
        "{}      {}",
        term::label("status:"),
        term::out().state(&r.status)
    );
    println!(
        "{}        {}",
        term::label("next:"),
        match (&r.status[..], r.next_micros) {
            ("paused", _) => "- (paused)".to_string(),
            (_, Some(next)) => format!("{} ({})", stamp(next), relative(next)),
            (_, None) => "never — the trigger names no future instant".to_string(),
        }
    );
    println!(
        "{}        {}",
        term::label("last:"),
        r.last_micros
            .map(|at| format!("{} ({})", stamp(at), relative(at)))
            .unwrap_or_else(|| "- (it has not fired yet)".into())
    );
    println!(
        "{}       {}",
        term::label("firer:"),
        r.firer
            .clone()
            .unwrap_or_else(|| "- (no live node is eligible; nothing is watching)".to_string())
    );
    println!(
        "{}     {} by {}",
        term::label("created:"),
        stamp(r.created_at_micros),
        r.created_by_username
    );
    Ok(())
}

pub fn print_schedule_check(v: &mesh_proto::ServiceScheduleCheck) -> Result<()> {
    println!("{}      {}", term::label("schedule:"), v.name);
    println!("{}       {}", term::label("service:"), v.service);
    println!("{}          {}", term::label("when:"), v.when);
    println!("{}       {}", term::label("targets:"), v.targets);
    println!(
        "{}   {} node{}",
        term::label("matches now:"),
        v.matching_nodes,
        if v.matching_nodes == 1 { "" } else { "s" }
    );
    println!(
        "{}    {}",
        term::label("would fire:"),
        match v.next_micros {
            Some(next) => format!("{} ({})", stamp(next), relative(next)),
            None => "never — the trigger names no future instant".to_string(),
        }
    );
    if v.issues.is_empty() {
        println!();
        println!("no problems — this schedule is ready to publish");
    } else {
        println!();
        println!("PROBLEMS (this will not be published as-is):");
        for issue in &v.issues {
            println!("  - {issue}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// N2 P1 — the port ledger
// ---------------------------------------------------------------------------

/// What this node can run, and what the rest of the mesh can (T4 §5).
///
/// Shaped after `nomad node status -verbose`, whose Drivers table is the
/// single most useful thing that output contains: one row per runtime,
/// detected or not, with the reason. The columns are deliberately the same
/// question in the same order — *can it*, *what did it find*, *what do I do
/// about it* — because that is the order somebody debugging asks them in.
pub fn print_runtimes(v: &mesh_proto::runtimes::RuntimesView, json: bool) -> Result<()> {
    use mesh_proto::runtimes::Detection;
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    println!("{}      {}", term::label("node:"), v.node);
    println!();
    // The fourth column is the remedy and is deliberately unheaded: a
    // "WHAT TO DO" header over a column that is empty on a healthy node
    // reads as something missing rather than as nothing to do.
    println!(
        "{}",
        term::out().dim(format!("{:<12} {:<10} {:<34}", "RUNTIME", "STATE", "FOUND"))
    );
    for r in &v.local {
        let state = match r.detection {
            Detection::Ready => "ready",
            Detection::Absent => "absent",
            Detection::Broken => "broken",
        };
        println!(
            "{} {} {:<34} {}",
            term::out().bold(format!("{:<12}", r.runtime.as_str())),
            term::out().state(format!("{:<10}", state)),
            truncate(&r.detail, 33),
            // The remedy on the same row as the problem. A table that makes
            // somebody look up what "absent" means about `ansible` has
            // failed at the one job it has.
            //
            // Its *headline*, because a row is one line: the long form goes
            // below, where paragraphs and shell commands can be read.
            truncate(r.headline(), 60)
        );
    }

    // The long form, under the table rather than inside it.
    //
    // Rootless podman's remedy is three paragraphs and a heredoc. Squeezed
    // into a 60-column cell it becomes an ellipsis; pushed into the log it
    // becomes an indented block truncated on the line that mattered. Here
    // it has the width of the terminal and appears once, in the command
    // somebody runs *because* they are debugging this.
    for r in &v.local {
        let Some(body) = r.explanation() else {
            continue;
        };
        println!();
        println!("── {} ─────────────────────────────", r.runtime.as_str());
        println!("{}", r.headline());
        println!();
        println!("{}", reflow(body));
    }

    if v.mesh.len() > 1 {
        println!();
        println!("{}", term::out().bold("Across the mesh:"));
        println!();
        println!("{}", term::out().dim(format!("{:<20} RUNTIMES", "NODE")));
        for n in &v.mesh {
            let list = match &n.runtimes {
                Some(rs) if rs.is_empty() => "none".to_string(),
                Some(rs) => rs.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", "),
                // Not "none": the two mean opposite things for placement,
                // and an operator looking at a job that went somewhere
                // surprising needs to know which they are seeing.
                None => "unknown (older build)".to_string(),
            };
            println!(
                "{} {}",
                term::out().bold(format!("{:<20}", truncate(&n.node, 19))),
                list
            );
        }
        let unknown = v.unknown_nodes();
        if !unknown.is_empty() {
            println!();
            let (noun, verb, pronoun) = if unknown.len() == 1 {
                ("node", "announces", "is")
            } else {
                ("nodes", "announce", "are")
            };
            println!(
                "{} {noun} {verb} no runtimes and {pronoun} treated as able to run anything: {}.",
                unknown.len(),
                unknown.join(", ")
            );
            println!("Upgrade them and this table stops guessing on their behalf.");
        }
    }
    println!();
    println!("Nothing here is configured. Install a tool and meshware finds it.");
    Ok(())
}

pub fn print_ports(v: &mesh_proto::PortLedgerView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    println!("{}      {}", term::label("node:"), v.node);
    match &v.range {
        Some(r) => println!(
            "{}     {}-{}  ({} free of {})",
            term::label("range:"),
            r.min,
            r.max,
            v.free,
            r.count()
        ),
        // Not "0 free": the two mean opposite things, and an operator
        // debugging a job that will not place needs to know which.
        None => println!(
            "{}     none advertised — this node takes no work that needs a port",
            term::label("range:")
        ),
    }
    if !v.reserved.is_empty() {
        println!(
            "{}  {}",
            term::label("reserved:"),
            v.reserved
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();
    if v.held.is_empty() {
        println!("nothing is holding a port");
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<8} {:<16} {:<6} HOLDER",
            "PORT", "LABEL", "KIND"
        ))
    );
    for h in &v.held {
        println!(
            "{} {:<16} {:<6} {}",
            term::out().bold(format!("{:<8}", h.port)),
            truncate(&h.label, 15),
            h.kind,
            h.holder
        );
    }
    println!();
    println!("A ledger is node-local: a dynamic port is chosen here, at spawn.");
    Ok(())
}

// ---------------------------------------------------------------------------
// N3 — the service registry
// ---------------------------------------------------------------------------

pub fn print_registry(
    rows: &[mesh_proto::RegistryRow],
    dns: Option<&mesh_proto::DnsStatusView>,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing is registered");
        println!();
        println!("A deployment registers with a [deployment.register] block; a one-off job");
        println!(
            "{}",
            term::out().prose("with `meshnode run --port http:9095 --register webapp`.")
        );
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<24} {:<12} {:<8} TAGS",
            "SERVICE", "HEALTHY", "NODES"
        ))
    );
    for r in rows {
        println!(
            "{} {:<12} {:<8} {}",
            term::out().bold(format!("{:<24}", truncate(&r.service, 23))),
            format!("{}/{}", r.healthy, r.total),
            r.nodes,
            r.tags.join(", ")
        );
    }
    println!();
    println!("HEALTHY shows: instances discovery will hand out / registered");
    if let Some(d) = dns {
        match (&d.bound, d.enabled) {
            (Some(addr), true) => println!(
                "{}",
                term::out().prose(&format!(
                    "DNS: {addr} — `dig @{} -p {} <service>.{}`",
                    addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
                    d.port,
                    d.suffix
                ))
            ),
            // Worth saying out loud: the registry is populated and nothing
            // will resolve, which looks like a broken service rather than a
            // turned-off responder.
            _ => println!("DNS: not answering — {}", d.detail),
        }
    }
    Ok(())
}

pub fn print_registry_service(
    service: &str,
    rows: &[mesh_proto::RegistryInstanceRow],
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing is registered as {service:?}");
        return Ok(());
    }
    println!("{}  {service}", term::label("service:"));
    println!(
        "resolves: {} of {} instance(s)",
        rows.iter().filter(|r| r.resolvable).count(),
        rows.len()
    );
    println!();
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<22} {:<10} {:<10} INSTANCE",
            "NODE", "ADDRESS", "LABEL", "HEALTH"
        ))
    );
    for r in rows {
        println!(
            "{} {:<22} {:<10} {} {}",
            term::out().bold(format!("{:<16}", truncate(&r.node, 15))),
            format!("{}:{}", r.address, r.port),
            truncate(&r.label, 9),
            term::out().state(format!(
                "{:<10}", // The one column a reader is actually scanning: an instance
                // that is registered and not handed out is the interesting row.
                if r.resolvable {
                    r.health.clone()
                } else {
                    format!("{} !", r.health)
                }
            )),
            r.instance.short()
        );
    }
    if rows.iter().any(|r| !r.resolvable) {
        println!();
        println!("! = registered but not handed out by discovery");
    }
    Ok(())
}

pub fn print_dns_status(v: &mesh_proto::DnsStatusView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    match (&v.bound, v.enabled) {
        // Phase A, §2.5d: a bound responder the probe found wrong — nothing
        // listening, or another meshnode's — carries its reason in
        // `detail`, and is not "answering" just because it is configured.
        (Some(addr), true) if !v.detail.is_empty() => {
            println!(
                "{}     NOT ANSWERING for this mesh on {addr}",
                term::label("dns:")
            );
            println!("{}     {}", term::label("why:"), v.detail);
        }
        (Some(addr), true) => {
            println!("{}     answering on {addr}", term::label("dns:"));
            println!(
                "{}    .{}  ({} name(s))",
                term::label("zone:"),
                v.suffix,
                v.names
            );
            if v.resolver_routed {
                println!("resolver: this machine sends .{} here", v.suffix);
            } else {
                println!("{}", term::out().prose(&format!("resolver: NOT routed — `curl <name>.{}` will not resolve on this machine until \
                     it is (`meshnode doctor` says how)",
                    v.suffix)));
            }
            println!(
                "{}",
                term::out().prose(
                    "answers: each healthy instance's node address; SRV carries its port \
                 (`meshnode registry <name>` lists both)"
                )
            );
            if v.privileged {
                println!(
                    "note:    bound below 1024 — this needs CAP_NET_BIND_SERVICE, which \
                     meshnode does not take by default"
                );
            } else {
                println!(
                    "note:    not port 53. Point a resolver at this, or see \
                     docs/operating/limits.md for the CAP_NET_BIND_SERVICE drop-in."
                );
            }
        }
        _ => println!("{}     not answering — {}", term::label("dns:"), v.detail),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// N5 — volumes
// ---------------------------------------------------------------------------

/// The mesh-wide shed (N6 §12).
///
/// Leads with the two switches, because that is the question. The
/// readiness line is second and is *not* optional decoration: this is the
/// one capability gate in meshware that fails open, so "shed on" and "shed
/// on and six nodes are ignoring it" are different states and the screen
/// must not let them look alike.
pub fn print_shed(v: &mesh_proto::ShedView, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    let mark = |on: bool| if on { "SHED" } else { "-   " };
    println!("{}  submissions   work people submit", mark(v.submissions));
    println!(
        "{}  reconcile     deployments and schedules starting work on their own",
        mark(v.reconcile)
    );
    if !v.submissions && !v.reconcile {
        println!();
        println!("Nothing is shed: this mesh takes work and reconciles normally.");
        return Ok(());
    }
    println!();
    if !v.reason.trim().is_empty() {
        println!("  {}   {}", term::label("reason"), v.reason.trim());
    }
    if let Some(name) = &v.by_name {
        println!(
            "  {}   {name}{}",
            term::label("set by"),
            if v.at_micros > 0 {
                format!(", {}", mesh_core::localtime::describe_instant(v.at_micros))
            } else {
                String::new()
            }
        );
    }
    println!(
        "  honoured by {} of {} nodes",
        v.readiness.honouring, v.readiness.members
    );
    if let Some(w) = v.readiness.warning() {
        println!();
        println!("  {w}");
    }
    println!();
    println!("Running jobs are unaffected — a shed is about what starts next.");
    println!("{}", term::out().prose("`meshnode shed clear` lifts it."));
    Ok(())
}

pub fn print_volumes(rows: &[mesh_proto::VolumeRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("this node lends no volumes");
        println!();
        println!("Declare one in meshnode.toml:");
        println!("  [[volume]]");
        println!("  name = \"data\"");
        println!("  path = \"/srv/data\"");
        return Ok(());
    }
    println!(
        "{}",
        term::out().dim(format!(
            "{:<16} {:<28} {:<16} {:<10} HELD BY",
            "VOLUME", "PATH", "SOURCE", "STATE"
        ))
    );
    for r in rows {
        // The one column a reader is scanning: can my job take it. A
        // read-only volume that nobody holds is still not writable, and
        // saying "free" there would be the table's only lie.
        let state = if r.read_only {
            "read-only".to_string()
        } else if r.write_held {
            "writing".to_string()
        } else if !r.holders.is_empty() {
            format!("{} reading", r.holders.len())
        } else {
            "free".to_string()
        };
        println!(
            "{} {:<28} {:<16} {} {}",
            term::out().bold(format!("{:<16}", truncate(&r.name, 15))),
            truncate(&r.path, 27),
            truncate(&r.source, 15),
            term::out().state(format!("{:<10}", state)),
            if r.holders.is_empty() {
                "-".to_string()
            } else {
                r.holders.join(", ")
            }
        );
    }
    println!();
    println!("A claim is a lease on this node, released the instant its job is gone —");
    println!("there is no counter anywhere else that could disagree with this table.");
    Ok(())
}

#[cfg(test)]
mod remedies {
    use super::*;

    /// **Prose wraps; commands do not.**
    ///
    /// A remedy is both, and a wrapped shell command is a command that
    /// cannot be pasted — which is the only thing the reader wanted from
    /// it. The convention the messages are written to is that an indented
    /// line is verbatim.
    #[test]
    fn a_command_line_survives_reflow_intact() {
        let cmd =
            "  sudo tee /etc/systemd/system/meshnode.service.d/containers.conf >/dev/null <<'EOF'";
        let body = format!(
            "Because the helper here is setuid-root, it becomes root and then finds that \
             euid 0 overrides no file permission at all without CAP_DAC_OVERRIDE, which \
             this unit clamps away.\n\n{cmd}\n[Service]\n"
        );
        let out = reflow(&body);
        assert!(
            out.lines().any(|l| l == cmd),
            "the command was reflowed:\n{out}"
        );
        // The paragraph did wrap, and to something readable.
        let widest = out.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(widest <= cmd.chars().count().max(86), "{widest}");
        assert!(out.lines().count() > 3, "nothing wrapped:\n{out}");
    }

    /// Blank lines separate paragraphs and have to survive, or the long
    /// form arrives as one wall of text — which is the complaint that
    /// started this.
    #[test]
    fn paragraph_breaks_are_kept() {
        let out = reflow("one\n\ntwo");
        assert_eq!(out, "one\n\ntwo");
    }
}
