//! What `meshnode run` puts on the terminal while it runs.
//!
//! **One machine** is the command's own bytes, untouched, between a header
//! and a footer — with a spinner until the first byte arrives, so a quiet
//! command does not look hung.
//!
//! **A fan-out**, on a terminal:
//!
//! ```text
//! ▸ ping -c 3 1.1.1.1  on 4 machines · fan-agqln2d4k7
//!
//!  IVY   ✓ succeeded · 2.1s
//!  │ PING 1.1.1.1 (1.1.1.1) 56(84) bytes of data.
//!  │ 64 bytes from 1.1.1.1: icmp_seq=1 ttl=57 time=11.8 ms
//!  │ …
//!
//! ── 1 of 4 finished · 2.2s ───────────────────────────────
//! ✓ IVY        2.1s  succeeded
//! ⠹ NIM              64 bytes from 1.1.1.1: icmp_seq=3 ttl=57 time=12.3 ms
//! ```
//!
//! Each machine's output is held until that machine finishes, then printed
//! as one block under a line saying how it ended — so output of several
//! lines reads as one piece per machine however the machines interleave.
//! Holding output is only acceptable because the board under it is live:
//! a running machine's row shows its latest line, so a long job is never a
//! blank screen. When the run ends the board becomes a table, and the
//! summary goes under it.
//!
//! No board means no holding: with stderr in a log or `TERM=dumb`, and
//! under `--stream`, lines are printed as they arrive, one lane per
//! machine, the name printed only when a different machine starts talking.
//!
//! **Off a terminal nothing changes a byte.** A pipe or a file gets
//! `[   IVY] line`, exactly as before, because scripts read it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use mesh_proto::JobState;

use crate::format::{self, Tone};
use crate::term::{self, BOLD, CYAN, Caps, DIM, HUES, Paint};

/// How much of one machine's output is held before part of its block is
/// printed early. Holding is for reading, and nobody reads a megabyte as
/// one block; past this it only costs memory.
const HOLD_CAP: usize = 1 << 20;

/// The spinner's frame time, and so how often the board redraws.
const TICK: Duration = Duration::from_millis(100);

/// Past this many rows the board shows only the machines still running.
const BOARD_MAX_ROWS: usize = 16;

/// The final table lists every machine up to this many; past it, only
/// the ones that did not succeed, and a count of the rest.
const TABLE_MAX_ROWS: usize = 16;

/// The widest a duration gets in `format::took`: `59m 59s`.
const TOOK_COLS: usize = 7;

/// One line of output, and the stream it goes to: 1 is stdout, 2 stderr.
type Out = (u8, String);

/// What is being fanned out, which changes a few words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanKind {
    Fanout,
    /// `--race`: every copy but the first answer is cancelled on purpose.
    Race,
    /// `--gang`, and how many offer rounds it took to start.
    Gang {
        rounds: u32,
    },
}

/// The terminal, for the life of one submission.
pub struct RunView {
    shared: Arc<Mutex<View>>,
    ticker: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for RunView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunView").finish_non_exhaustive()
    }
}

impl RunView {
    /// `title` is the command, for the header line; `stream` is
    /// `--stream`.
    pub fn new(title: Option<&str>, stream: bool) -> RunView {
        RunView {
            shared: Arc::new(Mutex::new(View::new(Caps::detect(), title, stream))),
            ticker: None,
        }
    }

    fn view(&self) -> MutexGuard<'_, View> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A status line on stderr, above anything live.
    pub fn say(&self, line: &str) {
        let mut v = self.view();
        let line = v.caps.text(line).into_owned();
        let _ = v.emit(vec![(2, line)]);
    }

    /// A status line that is context rather than news.
    pub fn note(&self, line: &str) {
        let mut v = self.view();
        let line = v.caps.err().sgr(DIM, &v.caps.text(line));
        let _ = v.emit(vec![(2, line)]);
    }

    /// Run `f` — anything that prints on its own — with the live region
    /// off the screen, and put it back after.
    pub fn aside<R>(&self, f: impl FnOnce() -> R) -> R {
        let mut v = self.view();
        v.close_open_line();
        v.erase_now();
        let r = f();
        v.after_blank = false;
        v.redraw();
        r
    }

    // -- one machine -----------------------------------------------------

    /// The job is running. `announce` is false under `--json`, where the
    /// header would be noise around the rows.
    pub fn job_started(&mut self, node: &str, job: &str, announce: bool) {
        let live = {
            let mut v = self.view();
            v.single = Some(Single {
                node: node.to_string(),
                started: Instant::now(),
                waiting: announce,
            });
            if announce {
                let header = v.single_header(node, job);
                let mut lines = vec![(2, header)];
                if v.caps.err.tty {
                    lines.push((2, String::new()));
                }
                let _ = v.emit(lines);
            }
            v.wants_live()
        };
        if live {
            self.start_ticker();
        }
    }

    /// The job's own bytes, as they are: a single job's output is the
    /// command's output, and nothing is put in front of it.
    pub fn job_output(&self, stream: u8, data: &[u8]) -> io::Result<()> {
        let mut v = self.view();
        if let Some(single) = v.single.as_mut() {
            single.waiting = false;
        }
        v.emit_raw(if stream == 1 { 1 } else { 2 }, data)
    }

    /// How the job ended, and where to look at it.
    pub fn job_done(&mut self, state: JobState, exit_code: Option<i32>, link: Option<&str>) {
        self.settle();
        let mut v = self.view();
        let mut lines = Vec::new();
        v.close_open_line();
        if v.caps.err.tty && !v.after_blank {
            lines.push((2, String::new()));
        }
        lines.push((2, v.single_footer(state, exit_code)));
        if let Some(url) = link
            && v.caps.err.tty
        {
            lines.push((2, v.caps.link_line(url)));
        }
        let _ = v.emit(lines);
    }

    // -- a fan-out -------------------------------------------------------

    /// The targets are known. `logs` is false under `--json`: the header
    /// is said, and nothing else is drawn.
    pub fn fan_started(&mut self, names: Vec<String>, id: &str, kind: FanKind, logs: bool) {
        let live = {
            let mut v = self.view();
            let fan = Fan::new(names, id, kind, logs, &v.caps, v.stream);
            let mut lines = vec![(2, v.fan_header(&fan))];
            v.fan = Some(fan);
            if logs && v.caps.err.tty {
                lines.push((2, String::new()));
            }
            let _ = v.emit(lines);
            v.wants_live()
        };
        if live {
            self.start_ticker();
        }
    }

    /// A gang's members, by rank: the one thing a gang's header has to say
    /// that a fan-out's does not.
    pub fn gang_ranks(&self, ranks: &[(String, String)]) {
        let mut v = self.view();
        let paint = v.caps.err();
        let lines = ranks
            .iter()
            .enumerate()
            .map(|(rank, (node, job))| {
                (2, paint.sgr(DIM, &format!("  rank {rank}  {node}  {job}")))
            })
            .collect();
        let _ = v.emit(lines);
    }

    pub fn fan_chunk(&self, index: u32, stream: u8, data: &[u8]) -> io::Result<()> {
        let mut v = self.view();
        let out = v.fan_chunk(index, stream, data);
        v.emit(out)
    }

    pub fn fan_job_done(
        &self,
        index: u32,
        state: JobState,
        exit_code: Option<i32>,
    ) -> io::Result<()> {
        let mut v = self.view();
        let out = v.fan_job_done(index, state, exit_code);
        v.emit(out)
    }

    /// The run is over: the live region comes off, output still held is
    /// printed, and the board is left behind as a table.
    pub fn settle(&mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        let mut v = self.view();
        let out = v.settle();
        let _ = v.emit(out);
    }

    /// The fan-out's closing lines, from the origin's counts and the names
    /// the frames gave.
    pub fn fan_summary(&self, succeeded: u32, failed: u32, skipped: u32) {
        let mut v = self.view();
        let out = v.fan_summary(succeeded, failed, skipped);
        let _ = v.emit(out);
    }

    pub fn race_summary(&self, succeeded: u32) {
        let mut v = self.view();
        let out = v.race_summary(succeeded);
        let _ = v.emit(out);
    }

    pub fn gang_summary(&self, id: &str, outcome: &str, ok: bool, succeeded: u32, failed: u32) {
        let mut v = self.view();
        let paint = v.caps.err();
        let took = v.fan.as_ref().map(|f| format::took(f.started.elapsed()));
        let (glyph, tone) = if ok {
            ("✓", Tone::Good)
        } else {
            ("✗", Tone::Bad)
        };
        let mut line = format!(
            "{} {}",
            paint.tone(tone, &v.caps.text(glyph)),
            paint.sgr(
                BOLD,
                &format!("gang {id} {outcome}: {succeeded} succeeded, {failed} failed")
            )
        );
        if let Some(took) = took {
            line.push_str(&paint.sgr(DIM, &v.caps.text(&format!(" · {took}"))));
        }
        let mut out = v.blank();
        out.push((2, line));
        let _ = v.emit(out);
    }

    /// A deep link as the last line — only to a terminal (see
    /// `format::print_deep_link`).
    pub fn link(&self, url: &str) {
        let mut v = self.view();
        if v.caps.err.tty {
            let line = v.caps.link_line(url);
            let _ = v.emit(vec![(2, line)]);
        }
    }

    fn start_ticker(&mut self) {
        if self.ticker.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.ticker = Some(tokio::spawn(async move {
            let mut every = tokio::time::interval(TICK);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                every.tick().await;
                let mut v = shared.lock().unwrap_or_else(PoisonError::into_inner);
                if v.wants_live() {
                    v.frame = v.frame.wrapping_add(1);
                    v.redraw();
                }
            }
        }));
    }
}

impl Drop for RunView {
    /// An error part-way through a run still leaves the terminal tidy: no
    /// spinner frozen mid-turn, and nothing held back unprinted.
    fn drop(&mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        let mut v = self.view();
        let out = v.settle();
        let _ = v.emit(out);
    }
}

// ---------------------------------------------------------------------------

/// Everything on screen, behind one lock: the ticker redraws the board
/// from it, and every write goes through it so a redraw can never land in
/// the middle of a block.
struct View {
    caps: Caps,
    title: Option<String>,
    stream: bool,
    /// Rows of the live region on screen now.
    drawn: usize,
    frame: usize,
    /// The last line put on the terminal was blank, so the next block or
    /// the board does not add a second.
    after_blank: bool,
    /// A single job's output ended without a newline on this stream; the
    /// footer must not be glued to it.
    open_line: Option<u8>,
    single: Option<Single>,
    fan: Option<Fan>,
    /// stderr's (columns, rows): `term::size`, or a fixed size in tests.
    size: fn() -> Option<(usize, usize)>,
}

struct Single {
    node: String,
    started: Instant,
    /// Nothing has come back yet: the spinner is showing.
    waiting: bool,
}

struct Fan {
    kind: FanKind,
    id: String,
    names: Vec<String>,
    hues: Vec<&'static str>,
    /// Columns of the widest name, for the lanes and the board.
    width: usize,
    /// The width `[{name:>w}]` has always used in a pipe — bytes, not
    /// columns — so that output stays byte-for-byte what it was.
    legacy_width: usize,
    started: Instant,
    logs: bool,
    /// Output bound for the terminal is held per machine until it
    /// finishes.
    group: bool,
    /// The board is drawn.
    live: bool,
    partial: HashMap<(u32, u8), Vec<u8>>,
    held: Vec<Vec<Out>>,
    held_bytes: Vec<usize>,
    /// Part of this machine's block was printed before it finished.
    shown_early: Vec<bool>,
    /// The latest line each machine sent, for its row on the board.
    last: Vec<String>,
    heard: Vec<bool>,
    done: Vec<Option<Done>>,
    /// Machines in the order they finished.
    order: Vec<u32>,
    /// Lanes: the machine whose name heads the lines just printed.
    speaker: Option<u32>,
    settled: bool,
}

#[derive(Debug, Clone, Copy)]
struct Done {
    state: JobState,
    exit_code: Option<i32>,
    took: Duration,
}

impl Fan {
    fn new(
        names: Vec<String>,
        id: &str,
        kind: FanKind,
        logs: bool,
        caps: &Caps,
        stream: bool,
    ) -> Fan {
        // Colours by alphabetical place, not by position in this run and
        // not by a hash: distinct within a run (a hash of four names into
        // six colours collides more often than not), and the same mesh
        // gets the same colours every time.
        let mut sorted: Vec<&String> = names.iter().collect();
        sorted.sort();
        sorted.dedup();
        let hues = names
            .iter()
            .map(|n| {
                let at = sorted.iter().position(|s| *s == n).unwrap_or(0);
                HUES[at % HUES.len()]
            })
            .collect();
        let n = names.len();
        let live = logs && caps.live;
        Fan {
            kind,
            id: id.to_string(),
            width: names.iter().map(|n| term::width(n)).max().unwrap_or(0),
            legacy_width: names.iter().map(String::len).max().unwrap_or(0),
            names,
            hues,
            started: Instant::now(),
            logs,
            group: live && caps.out.tty && !stream,
            live,
            partial: HashMap::new(),
            held: vec![Vec::new(); n],
            held_bytes: vec![0; n],
            shown_early: vec![false; n],
            last: vec![String::new(); n],
            heard: vec![false; n],
            done: vec![None; n],
            order: Vec::new(),
            speaker: None,
            settled: false,
        }
    }

    fn name(&self, index: u32) -> &str {
        self.names
            .get(index as usize)
            .map(String::as_str)
            .unwrap_or("?")
    }

    fn hue(&self, index: u32) -> &'static str {
        self.hues.get(index as usize).copied().unwrap_or(HUES[0])
    }

    fn finished(&self) -> usize {
        self.done.iter().filter(|d| d.is_some()).count()
    }

    /// A race's winner: the first copy to *finish* successfully. Not the
    /// first in target order, and not every success — a command as quick
    /// as `hostname` has every copy done before any can be cancelled, and
    /// only one of them was first.
    fn winner(&self) -> Option<u32> {
        self.order
            .iter()
            .copied()
            .find(|&i| self.done[i as usize].is_some_and(|d| d.state == JobState::Succeeded))
    }

    /// How machine `index` ended, in words.
    fn said(&self, index: u32, d: Done) -> (&'static str, Tone, String) {
        if self.kind == FanKind::Race
            && d.state == JobState::Succeeded
            && self.winner() != Some(index)
        {
            return ("✓", Tone::Good, "also succeeded".into());
        }
        outcome(d.state, d.exit_code, self.kind)
    }

    /// Whole lines out of a chunk; the rest waits for its newline.
    fn lines(&mut self, index: u32, stream: u8, data: &[u8]) -> Vec<String> {
        let buf = self.partial.entry((index, stream)).or_default();
        buf.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        lines
    }

    /// The dangling line of each stream, now that no newline is coming.
    fn tails(&mut self, index: u32) -> Vec<(u8, String)> {
        let mut out = Vec::new();
        for stream in [1u8, 2u8] {
            if let Some(buf) = self.partial.remove(&(index, stream))
                && !buf.is_empty()
            {
                out.push((stream, String::from_utf8_lossy(&buf).into_owned()));
            }
        }
        out
    }
}

/// The glyph, tone and words for how one job ended.
fn outcome(state: JobState, exit_code: Option<i32>, kind: FanKind) -> (&'static str, Tone, String) {
    let race = kind == FanKind::Race;
    match state {
        JobState::Succeeded if race => ("✓", Tone::Good, "answered first".into()),
        JobState::Succeeded => ("✓", Tone::Good, "succeeded".into()),
        JobState::Cancelled if race => ("–", Tone::Quiet, "lost the race, cancelled".into()),
        JobState::Cancelled => ("–", Tone::Quiet, "cancelled".into()),
        _ if format::is_no_answer(state, exit_code) => {
            ("?", Tone::Unknown, format::job_outcome(state, exit_code))
        }
        JobState::Failed => ("✗", Tone::Bad, format::job_outcome(state, exit_code)),
        _ => ("·", Tone::Quiet, format::job_outcome(state, exit_code)),
    }
}

impl View {
    fn new(caps: Caps, title: Option<&str>, stream: bool) -> View {
        View {
            caps,
            title: title.map(str::to_string),
            stream,
            drawn: 0,
            frame: 0,
            after_blank: false,
            open_line: None,
            single: None,
            fan: None,
            size: term::size,
        }
    }

    fn paint(&self, fd: u8) -> Paint {
        if fd == 1 {
            self.caps.out()
        } else {
            self.caps.err()
        }
    }

    fn tty(&self, fd: u8) -> bool {
        if fd == 1 {
            self.caps.out.tty
        } else {
            self.caps.err.tty
        }
    }

    /// A blank line on stderr, unless there is one already or stderr is a
    /// log, where spacing is only more lines to scroll.
    fn blank(&self) -> Vec<Out> {
        if self.caps.err.tty && !self.after_blank {
            vec![(2, String::new())]
        } else {
            Vec::new()
        }
    }

    // -- the live region --------------------------------------------------

    fn wants_live(&self) -> bool {
        if !self.caps.live {
            return false;
        }
        if let Some(fan) = &self.fan {
            return fan.live && !fan.settled;
        }
        self.single.as_ref().is_some_and(|s| s.waiting)
    }

    fn live_lines(&self, cols: usize, rows: usize) -> Vec<String> {
        if !self.wants_live() {
            return Vec::new();
        }
        if self.fan.is_some() {
            return self.board(cols, rows);
        }
        let Some(single) = &self.single else {
            return Vec::new();
        };
        let paint = self.caps.err();
        let spinner = self.caps.spinner();
        let line = format!(
            "running on {} · {}",
            single.node,
            format::took(single.started.elapsed())
        );
        vec![format!(
            "{} {}",
            paint.sgr(CYAN, spinner[self.frame % spinner.len()]),
            paint.sgr(
                DIM,
                &term::clip(
                    &self.caps.text(&line),
                    cols.saturating_sub(3),
                    self.caps.unicode
                )
            )
        )]
    }

    /// Take the live region off the screen, into `buf`.
    fn erase(&mut self, buf: &mut String) {
        if self.drawn > 0 {
            // Every row was drawn shorter than the terminal and ended with
            // a newline, so the cursor is at the start of the line below
            // the board: up that many rows, and clear to the end.
            buf.push_str(&format!("\r\x1b[{}A\x1b[J", self.drawn));
            self.drawn = 0;
        }
    }

    fn draw(&mut self, buf: &mut String) {
        if !self.wants_live() {
            return;
        }
        let (cols, rows) = (self.size)().unwrap_or((80, 24));
        let lines = self.live_lines(cols, rows);
        for line in &lines {
            buf.push_str(line);
            buf.push('\n');
        }
        self.drawn = lines.len();
    }

    fn erase_now(&mut self) {
        let mut buf = String::new();
        self.erase(&mut buf);
        write_err(&buf);
    }

    fn redraw(&mut self) {
        // One write, so the terminal never shows the board half-drawn.
        let mut buf = String::new();
        self.erase(&mut buf);
        self.draw(&mut buf);
        write_err(&buf);
    }

    /// A single job's output ended mid-line; end it before our own line.
    fn close_open_line(&mut self) {
        if let Some(fd) = self.open_line.take() {
            let _ = write_fd(fd, b"\n");
        }
    }

    /// Lines, each to its stream, with the live region lifted off the
    /// screen for them and put back after.
    fn emit(&mut self, lines: Vec<Out>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        self.close_open_line();
        let mut pre = String::new();
        self.erase(&mut pre);
        write_err(&pre);
        // Consecutive lines to one stream go in one write.
        let mut run: Option<(u8, String)> = None;
        let mut result = Ok(());
        for (fd, line) in lines {
            if self.tty(fd) {
                self.after_blank = line.is_empty();
            }
            match &mut run {
                Some((at, buf)) if *at == fd => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                _ => {
                    if let Some((at, buf)) = run.take() {
                        result = result.and(write_fd(at, buf.as_bytes()));
                    }
                    run = Some((fd, format!("{line}\n")));
                }
            }
        }
        if let Some((at, buf)) = run {
            result = result.and(write_fd(at, buf.as_bytes()));
        }
        let mut post = String::new();
        self.draw(&mut post);
        write_err(&post);
        result
    }

    fn emit_raw(&mut self, fd: u8, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut pre = String::new();
        self.erase(&mut pre);
        write_err(&pre);
        let result = write_fd(fd, data);
        if self.tty(fd) {
            self.after_blank = false;
            self.open_line = (!data.ends_with(b"\n")).then_some(fd);
        }
        let mut post = String::new();
        self.draw(&mut post);
        write_err(&post);
        result
    }

    // -- one machine ------------------------------------------------------

    fn title(&self, rest_cols: usize) -> Option<String> {
        let title = self.title.as_deref()?;
        let mut lines = title.lines();
        let first = lines.next().unwrap_or("").trim();
        let first = if lines.next().is_some() {
            format!("{first} …")
        } else {
            first.to_string()
        };
        // A header is one line on a terminal; in a log it can be as long
        // as the command.
        Some(match (self.size)().filter(|_| self.caps.err.tty) {
            Some((cols, _)) => term::clip(
                &first,
                cols.saturating_sub(rest_cols + 5).max(16),
                self.caps.unicode,
            ),
            None => first,
        })
    }

    fn single_header(&self, node: &str, job: &str) -> String {
        let paint = self.caps.err();
        let rest = format!(" on {node} · {job}");
        let arrow = paint.sgr(CYAN, &self.caps.text("▸"));
        let where_ = format!(
            "{}{}{}",
            paint.sgr(DIM, "on "),
            paint.sgr(HUES[0], node),
            paint.sgr(DIM, &self.caps.text(&format!(" · {job}")))
        );
        match self.title(term::width(&rest)) {
            Some(title) => format!("{arrow} {}  {where_}", paint.sgr(BOLD, &title)),
            None => format!("{arrow} {}{where_}", paint.sgr(DIM, "running ")),
        }
    }

    fn single_footer(&self, state: JobState, exit_code: Option<i32>) -> String {
        let paint = self.caps.err();
        let (glyph, tone, said) = outcome(state, exit_code, FanKind::Fanout);
        let mut line = format!(
            "{} {}",
            paint.tone(tone, &self.caps.text(glyph)),
            paint.tone(tone, &self.caps.text(&said))
        );
        if let Some(single) = &self.single {
            line.push_str(&paint.sgr(DIM, &self.caps.text(" · ")));
            line.push_str(&paint.sgr(HUES[0], &single.node));
            line.push_str(
                &paint.sgr(
                    DIM,
                    &self
                        .caps
                        .text(&format!(" · {}", format::took(single.started.elapsed()))),
                ),
            );
        }
        line
    }

    // -- a fan-out --------------------------------------------------------

    fn fan_header(&self, fan: &Fan) -> String {
        let paint = self.caps.err();
        let n = fan.names.len();
        let machines = format!("{n} machine{}", if n == 1 { "" } else { "s" });
        let rest = match fan.kind {
            FanKind::Fanout => format!("on {machines} · {}", fan.id),
            FanKind::Race => format!("racing {machines} · {}", fan.id),
            FanKind::Gang { rounds } => format!(
                "gang {} · {n} member{} started together{}",
                fan.id,
                if n == 1 { "" } else { "s" },
                if rounds > 1 {
                    format!(" after {rounds} offer rounds")
                } else {
                    String::new()
                }
            ),
        };
        let arrow = paint.sgr(CYAN, &self.caps.text("▸"));
        match self.title(term::width(&rest)) {
            Some(title) => format!(
                "{arrow} {}  {}",
                paint.sgr(BOLD, &title),
                paint.sgr(DIM, &self.caps.text(&rest))
            ),
            None => format!(
                "{arrow} {}",
                paint.sgr(DIM, &self.caps.text(&format!("running {rest}")))
            ),
        }
    }

    /// A machine's name in its colour, padded to the lane.
    fn name_cell(&self, fan: &Fan, index: u32, fd: u8) -> String {
        let name = fan.name(index);
        format!(
            "{}{}",
            self.paint(fd).sgr(fan.hue(index), name),
            " ".repeat(fan.width.saturating_sub(term::width(name)))
        )
    }

    fn gutter(&self, fd: u8) -> String {
        self.paint(fd)
            .sgr(DIM, &self.caps.text(if fd == 1 { " │ " } else { " ┃ " }))
    }

    /// A block's name badge, padded to the widest name so that how each
    /// machine ended lines up in one column down the screen.
    fn chip(&self, fan: &Fan, index: u32) -> String {
        let name = fan.name(index);
        let pad = " ".repeat(fan.width.saturating_sub(term::width(name)));
        if self.caps.err.color {
            self.caps.err().sgr(
                &format!("1;7;{}", fan.hue(index)),
                &format!(" {name}{pad} "),
            )
        } else {
            format!("[{name}]{pad}")
        }
    }

    fn fan_chunk(&mut self, index: u32, stream: u8, data: &[u8]) -> Vec<Out> {
        let Some(mut fan) = self.fan.take() else {
            return Vec::new();
        };
        let lines = fan.lines(index, stream, data);
        let out = self.fan_lines(&mut fan, index, stream, lines);
        self.fan = Some(fan);
        out
    }

    fn fan_lines(&self, fan: &mut Fan, index: u32, stream: u8, lines: Vec<String>) -> Vec<Out> {
        let i = index as usize;
        let fd = if stream == 1 { 1 } else { 2 };
        let mut out = Vec::new();
        if i >= fan.names.len() || !fan.logs {
            return out;
        }
        for line in lines {
            fan.heard[i] = true;
            let shown = term::plain(&line);
            if !shown.is_empty() {
                fan.last[i] = shown;
            }
            if !self.tty(fd) {
                // A pipe or a file: today's bytes, exactly.
                out.push((
                    fd,
                    format!("[{:>w$}] {line}", fan.name(index), w = fan.legacy_width),
                ));
            } else if fan.group {
                fan.held_bytes[i] += line.len();
                fan.held[i].push((fd, line));
                if fan.held_bytes[i] > HOLD_CAP {
                    out.extend(self.block(fan, index, None));
                }
            } else {
                let name = if fan.speaker == Some(index) {
                    " ".repeat(fan.width)
                } else {
                    self.name_cell(fan, index, fd)
                };
                fan.speaker = Some(index);
                out.push((fd, format!("{name}{}{line}", self.gutter(fd))));
            }
        }
        out
    }

    /// A machine's block: a line saying how it ended (or that it has not
    /// yet), then everything it said. `done` is None for an early block.
    fn block(&self, fan: &mut Fan, index: u32, done: Option<Done>) -> Vec<Out> {
        let i = index as usize;
        let paint = self.caps.err();
        let mut out = Vec::new();
        let head = match done {
            Some(d) => {
                let (glyph, tone, said) = fan.said(index, d);
                let mut head = format!(
                    "{}  {} {}{}",
                    self.chip(fan, index),
                    paint.tone(tone, &self.caps.text(glyph)),
                    paint.tone(tone, &self.caps.text(&said)),
                    paint.sgr(
                        DIM,
                        &self.caps.text(&format!(" · {}", format::took(d.took)))
                    )
                );
                if fan.shown_early[i] {
                    head.push_str(&paint.sgr(DIM, &self.caps.text(" · continued from above")));
                }
                head
            }
            None => format!(
                "{}  {}",
                self.chip(fan, index),
                paint.sgr(
                    DIM,
                    &self.caps.text(&format!(
                        "still running · {} · the rest follows when it finishes",
                        format::took(fan.started.elapsed())
                    ))
                )
            ),
        };
        out.push((2, head));
        for (fd, line) in std::mem::take(&mut fan.held[i]) {
            out.push((fd, format!("{}{line}", self.gutter(fd))));
        }
        fan.held_bytes[i] = 0;
        if done.is_none() {
            fan.shown_early[i] = true;
        }
        // The blank that ends a block goes where its lines went.
        out.push((if self.caps.out.tty { 1 } else { 2 }, String::new()));
        out
    }

    fn fan_job_done(&mut self, index: u32, state: JobState, exit_code: Option<i32>) -> Vec<Out> {
        let Some(mut fan) = self.fan.take() else {
            return Vec::new();
        };
        let i = index as usize;
        if i >= fan.names.len() {
            self.fan = Some(fan);
            return Vec::new();
        }
        let mut out = Vec::new();
        // Terminate the dangling line so it goes out with its machine.
        for (stream, line) in fan.tails(index) {
            out.extend(self.fan_lines(&mut fan, index, stream, vec![line]));
        }
        let done = Done {
            state,
            exit_code,
            took: fan.started.elapsed(),
        };
        fan.done[i] = Some(done);
        fan.order.push(index);
        if fan.logs {
            if fan.group {
                out.extend(self.block(&mut fan, index, Some(done)));
            } else if !fan.live {
                // No board to show it, so it is said where it happened.
                let paint = self.caps.err();
                let (glyph, tone, said) = fan.said(index, done);
                out.push((
                    2,
                    format!(
                        "{} {} {}{}",
                        self.name_cell(&fan, index, 2),
                        paint.tone(tone, &self.caps.text(glyph)),
                        paint.tone(tone, &self.caps.text(&said)),
                        paint.sgr(
                            DIM,
                            &self.caps.text(&format!(" · {}", format::took(done.took)))
                        )
                    ),
                ));
                fan.speaker = None;
            }
        }
        self.fan = Some(fan);
        out
    }

    /// One machine's row, on the board (`frame` given) or in the table.
    fn row(&self, fan: &Fan, index: u32, cols: usize, frame: Option<usize>) -> String {
        let i = index as usize;
        let paint = self.caps.err();
        // mark, space, name, two spaces, duration, two spaces
        let room = cols.saturating_sub(1 + 1 + 1 + fan.width + 2 + TOOK_COLS + 2);
        let name = self.name_cell(fan, index, 2);
        match fan.done[i] {
            Some(d) => {
                let (glyph, tone, said) = fan.said(index, d);
                let said = term::clip(&self.caps.text(&said), room, self.caps.unicode);
                format!(
                    "{} {name}  {}  {}",
                    paint.tone(tone, &self.caps.text(glyph)),
                    paint.sgr(DIM, &format!("{:>TOOK_COLS$}", format::took(d.took))),
                    if d.state == JobState::Succeeded {
                        said
                    } else {
                        paint.tone(tone, &said)
                    }
                )
            }
            None => {
                let mark = match frame {
                    Some(f) => {
                        let spinner = self.caps.spinner();
                        paint.sgr(CYAN, spinner[(f + i * 3) % spinner.len()])
                    }
                    None => paint.sgr(DIM, &self.caps.text("·")),
                };
                // The latest line only when the output itself is being
                // held: in lanes it is already on screen, and when stdout
                // went to a file the person chose not to see it here.
                let said = if frame.is_none() {
                    "no result".to_string()
                } else if fan.group && !fan.last[i].is_empty() {
                    fan.last[i].clone()
                } else if fan.heard[i] {
                    "running".to_string()
                } else {
                    "no output yet".to_string()
                };
                format!(
                    "{mark} {name}  {}  {}",
                    " ".repeat(TOOK_COLS),
                    paint.sgr(DIM, &term::clip(&said, room, self.caps.unicode))
                )
            }
        }
    }

    fn board(&self, cols: usize, rows: usize) -> Vec<String> {
        let Some(fan) = &self.fan else {
            return Vec::new();
        };
        let paint = self.caps.err();
        let n = fan.names.len();
        let mut lines = Vec::new();
        if !self.after_blank {
            lines.push(String::new());
        }

        let mut head = format!("── {} of {n} finished", fan.finished());
        let failed = fan
            .done
            .iter()
            .flatten()
            .filter(|d| d.state == JobState::Failed && d.exit_code.is_some())
            .count();
        let unknown = fan
            .done
            .iter()
            .flatten()
            .filter(|d| format::is_no_answer(d.state, d.exit_code))
            .count();
        if failed > 0 {
            head.push_str(&format!(" · ✗ {failed}"));
        }
        if unknown > 0 {
            head.push_str(&format!(" · ? {unknown}"));
        }
        head.push_str(&format!(" · {} ", format::took(fan.started.elapsed())));
        let head = self.caps.text(&head).into_owned();
        let rule = cols.min(80).saturating_sub(1 + term::width(&head));
        let dash = self.caps.text("─");
        lines.push(paint.sgr(DIM, &format!("{head}{}", dash.repeat(rule))));

        let cols = cols.saturating_sub(1);
        let fit = rows
            .saturating_sub(lines.len() + 2)
            .clamp(1, BOARD_MAX_ROWS);
        if n <= fit {
            for i in 0..n {
                lines.push(self.row(fan, i as u32, cols, Some(self.frame)));
            }
        } else {
            // Too many to show: the machines still running, which are the
            // ones a person is waiting on. The finished ones are in the
            // blocks above and the counts on the rule.
            let running: Vec<usize> = (0..n).filter(|i| fan.done[*i].is_none()).collect();
            let shown = running.len().min(fit.saturating_sub(1).max(1));
            for &i in &running[..shown] {
                lines.push(self.row(fan, i as u32, cols, Some(self.frame)));
            }
            if running.len() > shown {
                lines.push(
                    paint.sgr(
                        DIM,
                        &self
                            .caps
                            .text(&format!("  … and {} more running", running.len() - shown)),
                    ),
                );
            }
        }
        lines
    }

    fn settle(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(mut fan) = self.fan.take() else {
            if let Some(single) = self.single.as_mut() {
                single.waiting = false;
            }
            return out;
        };
        if fan.settled {
            self.fan = Some(fan);
            return out;
        }
        fan.settled = true;
        // Output still held for a machine that never finished — the stream
        // broke, or the origin gave up on it — is printed, not dropped.
        for index in 0..fan.names.len() as u32 {
            let tails = fan.tails(index);
            for (stream, line) in tails {
                out.extend(self.fan_lines(&mut fan, index, stream, vec![line]));
            }
            if !fan.held[index as usize].is_empty() {
                out.extend(self.block(&mut fan, index, None));
            }
        }
        if fan.live {
            // The board, left behind as the record of the run.
            let cols = (self.size)().map_or(100, |(c, _)| c).saturating_sub(1);
            if self.caps.err.tty
                && !out.last().is_some_and(|(_, l)| l.is_empty())
                && !self.after_blank
            {
                out.push((2, String::new()));
            }
            let n = fan.names.len();
            let paint = self.caps.err();
            let ok = fan
                .done
                .iter()
                .flatten()
                .filter(|d| d.state == JobState::Succeeded)
                .count();
            for i in 0..n {
                let succeeded = fan.done[i].is_some_and(|d| d.state == JobState::Succeeded);
                if n <= TABLE_MAX_ROWS || !succeeded {
                    out.push((2, self.row(&fan, i as u32, cols, None)));
                }
            }
            if n > TABLE_MAX_ROWS && ok > 0 {
                out.push((
                    2,
                    format!(
                        "{} {}",
                        paint.tone(Tone::Good, &self.caps.text("✓")),
                        paint.sgr(DIM, &format!("{ok} more succeeded"))
                    ),
                ));
            }
        }
        self.fan = Some(fan);
        out
    }

    fn fan_summary(&self, succeeded: u32, failed: u32, skipped: u32) -> Vec<Out> {
        let Some(fan) = &self.fan else {
            return Vec::new();
        };
        let mut t = format::FanTally {
            succeeded,
            skipped,
            took: fan.started.elapsed(),
            ..format::FanTally::default()
        };
        for (i, d) in fan.done.iter().enumerate() {
            let Some(d) = d else { continue };
            if format::is_no_answer(d.state, d.exit_code) {
                t.no_answer.push(fan.names[i].clone());
            } else if d.state == JobState::Cancelled {
                t.cancelled += 1;
            } else if d.exit_code.is_some_and(|c| c != 0) {
                t.exited_nonzero.push(fan.names[i].clone());
            }
        }
        // `failed` is the origin's count and is the authority on how many
        // went wrong; the frames are the authority on *which kind*. When a
        // target never sent a terminal frame at all — the origin gave up on
        // it — the difference is no answer, which is exactly what it was.
        let accounted = (t.exited_nonzero.len() + t.no_answer.len()) as u32 + t.cancelled;
        t.no_answer_unnamed = failed.saturating_sub(accounted);
        let paint = self.caps.err();
        let mut out = self.blank();
        for spans in format::fanout_outcome_lines(&t) {
            let spans: Vec<(Tone, String)> = spans
                .into_iter()
                .map(|(tone, s)| (tone, self.caps.text(&s).into_owned()))
                .collect();
            out.push((2, paint.line(&spans)));
        }
        out
    }

    fn race_summary(&self, succeeded: u32) -> Vec<Out> {
        let Some(fan) = &self.fan else {
            return Vec::new();
        };
        let paint = self.caps.err();
        let copies = |n: usize| format!("{n} other cop{}", if n == 1 { "y" } else { "ies" });
        let spans = match fan.winner().filter(|_| succeeded > 0) {
            Some(w) => {
                let took = fan.done[w as usize].map_or(fan.started.elapsed(), |d| d.took);
                let mut spans = vec![
                    (Tone::Good, "✓ ".to_string()),
                    (Tone::Strong, format!("{} answered first", fan.name(w))),
                    (Tone::Quiet, format!(" · {}", format::took(took))),
                ];
                let count = |f: &dyn Fn(&Done) -> bool| {
                    fan.done
                        .iter()
                        .enumerate()
                        .filter(|(i, d)| *i != w as usize && d.as_ref().is_some_and(f))
                        .count()
                };
                let cancelled = count(&|d| d.state == JobState::Cancelled);
                let also = count(&|d| d.state == JobState::Succeeded);
                let failed = fan.names.len() - 1 - cancelled - also;
                for (n, what) in [
                    (cancelled, "cancelled"),
                    (also, "finished too"),
                    (failed, "did not succeed"),
                ] {
                    if n > 0 {
                        spans.push((Tone::Quiet, format!(" · {} {what}", copies(n))));
                    }
                }
                spans
            }
            None => vec![
                (Tone::Bad, "✗ ".to_string()),
                (Tone::Strong, "Race lost: every copy failed".to_string()),
                (
                    Tone::Quiet,
                    format!(" · {}", format::took(fan.started.elapsed())),
                ),
            ],
        };
        let spans: Vec<(Tone, String)> = spans
            .into_iter()
            .map(|(tone, s)| (tone, self.caps.text(&s).into_owned()))
            .collect();
        let mut out = self.blank();
        out.push((2, paint.line(&spans)));
        out
    }
}

fn write_fd(fd: u8, bytes: &[u8]) -> io::Result<()> {
    if fd == 1 {
        let mut out = io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()
    } else {
        let mut err = io::stderr().lock();
        err.write_all(bytes)?;
        err.flush()
    }
}

/// The live region's own writes. A failure here costs a frame of the
/// board, never the run.
fn write_err(buf: &str) {
    if !buf.is_empty() {
        let _ = write_fd(2, buf.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Stream;

    fn caps(out_tty: bool, err_tty: bool) -> Caps {
        Caps {
            out: Stream {
                tty: out_tty,
                color: false,
            },
            err: Stream {
                tty: err_tty,
                color: false,
            },
            live: err_tty,
            unicode: true,
            links: false,
        }
    }

    fn view(out_tty: bool, err_tty: bool, stream: bool) -> View {
        let mut v = View::new(caps(out_tty, err_tty), Some("ping -c 3 1.1.1.1"), stream);
        v.size = || Some((100, 40));
        let names = ["IVY", "NIM", "merope", "APUS"].map(String::from).to_vec();
        let c = v.caps;
        v.fan = Some(Fan::new(
            names,
            "fan-agqln2d4k7",
            FanKind::Fanout,
            true,
            &c,
            stream,
        ));
        v
    }

    fn text(out: &[Out]) -> Vec<String> {
        out.iter().map(|(fd, l)| format!("{fd}:{l}")).collect()
    }

    #[test]
    fn a_pipe_gets_exactly_what_it_always_got() {
        // stdout to a file, stderr to the screen.
        let mut v = view(false, true, false);
        let out = v.fan_chunk(2, 1, b"Filesystem  Size\n/dev/sda1  1T\n");
        assert_eq!(
            text(&out),
            ["1:[merope] Filesystem  Size", "1:[merope] /dev/sda1  1T"]
        );
        let out = v.fan_chunk(0, 1, b"one\n");
        assert_eq!(text(&out), ["1:[   IVY] one"]);
    }

    #[test]
    fn on_a_terminal_a_machines_output_waits_for_its_block() {
        let mut v = view(true, true, false);
        assert!(v.fan_chunk(0, 1, b"PING 1.1.1.1\n").is_empty());
        assert!(v.fan_chunk(1, 1, b"PING 1.1.1.1\n").is_empty());
        assert!(v.fan_chunk(0, 2, b"a warning\n").is_empty());
        assert!(v.fan_chunk(0, 1, b"no newline yet").is_empty());
        let out = v.fan_job_done(0, JobState::Succeeded, Some(0));
        let said = text(&out);
        // Padded to "merope", the widest name.
        assert!(
            said[0].starts_with("2:[IVY]     ✓ succeeded · "),
            "{said:?}"
        );
        assert_eq!(
            &said[1..],
            [
                "1: │ PING 1.1.1.1",
                "2: ┃ a warning",
                "1: │ no newline yet",
                "1:",
            ]
        );
        // NIM's line is still held: it has not finished.
        assert!(v.fan.as_ref().is_some_and(|f| f.held[1].len() == 1));
    }

    #[test]
    fn a_running_machine_shows_its_latest_line_on_the_board() {
        let mut v = view(true, true, false);
        v.after_blank = true;
        v.fan_chunk(1, 1, b"64 bytes from 1.1.1.1: icmp_seq=2\n\n");
        v.fan_job_done(0, JobState::Succeeded, Some(0));
        let board = v.board(100, 40);
        assert!(board[0].starts_with("── 1 of 4 finished · "), "{board:?}");
        assert!(board[1].starts_with("✓ IVY   "), "{board:?}");
        assert!(board[1].ends_with("  succeeded"), "{board:?}");
        // The blank line after it is not "latest": the last thing said is.
        assert!(
            board[2].ends_with("64 bytes from 1.1.1.1: icmp_seq=2"),
            "{board:?}"
        );
        assert!(board[3].ends_with("no output yet"), "{board:?}");
        assert_eq!(board.len(), 5, "a rule and a row per machine: {board:?}");
    }

    #[test]
    fn a_crowded_board_shows_whats_still_running() {
        let mut v = View::new(caps(true, true), None, false);
        let names: Vec<String> = (0..40).map(|i| format!("node{i:02}")).collect();
        let c = v.caps;
        v.fan = Some(Fan::new(names, "fan-x", FanKind::Fanout, true, &c, false));
        for i in 0..30 {
            v.fan_job_done(i, JobState::Succeeded, Some(0));
        }
        v.after_blank = true;
        let board = v.board(100, 12);
        assert!(board[0].starts_with("── 30 of 40 finished"), "{board:?}");
        // A 12-row terminal: the rule, eight of the ten still running, and
        // a line for the other two.
        assert_eq!(board.len(), 10, "{board:?}");
        assert!(board[1].contains("node30"), "{board:?}");
        assert!(board[9].contains("… and 2 more running"), "{board:?}");
    }

    #[test]
    fn stream_mode_names_a_machine_once_per_turn() {
        let mut v = view(true, true, true);
        let mut out = v.fan_chunk(0, 1, b"one\ntwo\n");
        out.extend(v.fan_chunk(2, 1, b"three\n"));
        out.extend(v.fan_chunk(0, 1, b"four\n"));
        assert_eq!(
            text(&out),
            [
                "1:IVY    │ one",
                "1:       │ two",
                "1:merope │ three",
                "1:IVY    │ four",
            ]
        );
    }

    #[test]
    fn without_a_board_each_ending_is_said_where_it_happens() {
        // stderr to a log: no board, so no holding either.
        let mut v = view(true, false, false);
        let out = v.fan_chunk(1, 1, b"hello\n");
        assert_eq!(text(&out), ["1:NIM    │ hello"]);
        let out = v.fan_job_done(1, JobState::Failed, Some(3));
        assert_eq!(out.len(), 1);
        assert!(
            out[0].1.starts_with("NIM    ✗ your command exited 3 · "),
            "{out:?}"
        );
    }

    #[test]
    fn settling_leaves_the_table_and_prints_what_was_held() {
        let mut v = view(true, true, false);
        v.fan_job_done(0, JobState::Succeeded, Some(0));
        v.fan_job_done(1, JobState::Failed, Some(1));
        v.fan_job_done(3, JobState::Failed, None);
        // merope never finished, and said something first.
        v.fan_chunk(2, 1, b"partial\n");
        let said = text(&v.settle());
        let at = said
            .iter()
            .position(|l| l.contains("[merope]  still running"))
            .expect("merope's held output is printed");
        assert_eq!(said[at + 1], "1: │ partial");
        let table: Vec<&String> = said
            .iter()
            .filter(|l| l.starts_with("2:") && l.len() > 2)
            .collect();
        let rows = &table[table.len() - 4..];
        assert!(rows[0].starts_with("2:✓ IVY "), "{rows:?}");
        assert!(rows[1].contains("your command exited 1"), "{rows:?}");
        assert!(rows[2].ends_with("no result"), "{rows:?}");
        assert!(rows[3].contains("no answer"), "{rows:?}");
        // Settling twice prints nothing twice.
        assert!(v.settle().is_empty());
    }

    #[test]
    fn the_summary_names_names() {
        let mut v = view(true, true, false);
        v.fan_job_done(0, JobState::Succeeded, Some(0));
        v.fan_job_done(1, JobState::Failed, Some(1));
        v.fan_job_done(2, JobState::Succeeded, Some(0));
        v.fan_job_done(3, JobState::Failed, None);
        let said = text(&v.fan_summary(2, 2, 0));
        let all = said.join("\n");
        assert!(
            all.contains("✗ 2 of 4 succeeded · 1 exited non-zero · 1 never answered"),
            "{all}"
        );
        assert!(all.contains("Your command failed on NIM"), "{all}");
        assert!(all.contains("an answer from APUS"), "{all}");
    }

    #[test]
    fn a_race_names_its_winner() {
        let mut v = View::new(caps(true, true), None, false);
        let names = ["IVY", "NIM", "APUS"].map(String::from).to_vec();
        let c = v.caps;
        v.fan = Some(Fan::new(names, "fan-r", FanKind::Race, true, &c, false));
        v.fan_job_done(1, JobState::Succeeded, Some(0));
        v.fan_job_done(0, JobState::Cancelled, None);
        v.fan_job_done(2, JobState::Cancelled, None);
        let said = text(&v.race_summary(1)).join("\n");
        assert!(said.contains("✓ NIM answered first · "), "{said}");
        assert!(said.ends_with(" · 2 other copies cancelled"), "{said}");
    }

    #[test]
    fn a_race_everyone_finished_has_one_winner() {
        // `hostname` is quicker than a cancel: every copy succeeds, and
        // the one that finished first — not the first listed — won.
        let mut v = View::new(caps(true, true), None, false);
        v.size = || Some((100, 40));
        let names = ["IVY", "NIM", "merope"].map(String::from).to_vec();
        let c = v.caps;
        v.fan = Some(Fan::new(names, "fan-r", FanKind::Race, true, &c, false));
        let first = text(&v.fan_job_done(1, JobState::Succeeded, Some(0)));
        let second = text(&v.fan_job_done(0, JobState::Succeeded, Some(0)));
        assert!(first[0].contains("answered first"), "{first:?}");
        assert!(second[0].contains("also succeeded"), "{second:?}");
        v.fan_job_done(2, JobState::Succeeded, Some(0));
        let said = text(&v.race_summary(3)).join("\n");
        assert!(said.contains("NIM answered first"), "{said}");
        assert!(said.ends_with(" · 2 other copies finished too"), "{said}");
    }

    #[test]
    fn the_header_says_what_where() {
        let v = view(true, true, false);
        let fan = v.fan.as_ref().expect("fan");
        assert_eq!(
            v.fan_header(fan),
            "▸ ping -c 3 1.1.1.1  on 4 machines · fan-agqln2d4k7"
        );
        let mut v = View::new(caps(true, true), None, false);
        v.size = || Some((100, 40));
        assert_eq!(
            v.single_header("NIM", "job-agqlm4c5f2"),
            "▸ running on NIM · job-agqlm4c5f2"
        );
    }
}
