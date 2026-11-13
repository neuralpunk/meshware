//! What the terminal on the other end of stdout and stderr can take.
//!
//! Four questions, each answered once per process and never guessed at by
//! a renderer:
//!
//! - **Colour, per stream.** stdout and stderr are asked separately, so
//!   `meshnode run … > out.txt` writes plain bytes to the file and still
//!   colours the status lines on the screen.
//! - **Live drawing.** Whether a region of stderr may be redrawn in place —
//!   the fan-out board, a spinner. Needs a terminal that moves its cursor,
//!   which `TERM=dumb` says it does not.
//! - **Glyphs.** `✓`, `─` and the braille spinner need a UTF-8 terminal. A
//!   locale that says otherwise gets an ASCII stand-in for every one.
//! - **Links.** Whether a `director://` line can also be an OSC 8
//!   hyperlink. Only where the terminal is known to understand one: an
//!   unknown escape is ignored by most terminals and printed as garbage by
//!   the rest.
//!
//! Colour follows the conventions people already set once for every tool:
//! `NO_COLOR` turns it off, `CLICOLOR_FORCE` turns it on in a pipe, and
//! `--color` beats both. Every colour is one of the terminal's own sixteen,
//! so it follows the theme — a fixed 24-bit colour chosen on a dark
//! background is unreadable on a light one.

use std::borrow::Cow;
use std::io::IsTerminal as _;
use std::sync::OnceLock;

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::format::Tone;

/// `--color`: what the person asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour on a terminal, plain text anywhere else
    #[default]
    Auto,
    /// Colour even into a pipe or a file
    Always,
    /// Never colour
    Never,
}

static CHOICE: OnceLock<ColorChoice> = OnceLock::new();

/// Record `--color` for the rest of the process. Set once, from `main`,
/// before anything prints.
pub fn set_choice(choice: ColorChoice) {
    let _ = CHOICE.set(choice);
}

/// One stream's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    /// A person is reading it, not a file or a pipe.
    pub tty: bool,
    /// It gets SGR colour and weight.
    pub color: bool,
}

/// Everything a renderer needs to know about the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub out: Stream,
    pub err: Stream,
    /// stderr may be redrawn in place.
    pub live: bool,
    /// The terminal shows `✓`, `─` and `⠋`; when not, [`Caps::text`]
    /// swaps each for ASCII.
    pub unicode: bool,
    /// A link on stderr may be an OSC 8 hyperlink.
    pub links: bool,
}

impl Caps {
    /// Read the real terminal and environment.
    pub fn detect() -> Caps {
        Caps::decide(
            &|key| std::env::var(key).ok(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
            CHOICE.get().copied().unwrap_or_default(),
        )
    }

    /// The decision itself, over an environment the tests can hand in.
    fn decide(
        env: &dyn Fn(&str) -> Option<String>,
        out_tty: bool,
        err_tty: bool,
        choice: ColorChoice,
    ) -> Caps {
        let set = |key: &str| env(key).is_some_and(|v| !v.is_empty());
        let dumb = env("TERM").as_deref() == Some("dumb");
        let color = |tty: bool| match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            // no-color.org: any non-empty value. CLICOLOR_FORCE after it,
            // so a machine-wide NO_COLOR is not undone by a stray force.
            ColorChoice::Auto if set("NO_COLOR") => false,
            ColorChoice::Auto
                if env("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") =>
            {
                true
            }
            ColorChoice::Auto if env("CLICOLOR").as_deref() == Some("0") => false,
            ColorChoice::Auto => tty && !dumb,
        };
        Caps {
            out: Stream {
                tty: out_tty,
                color: color(out_tty),
            },
            err: Stream {
                tty: err_tty,
                color: color(err_tty),
            },
            // Terminal size comes from an ioctl, and a board drawn without
            // knowing the width wraps and cannot be erased cleanly.
            live: err_tty && !dumb && cfg!(unix),
            unicode: !dumb && locale_is_utf8(env),
            links: err_tty && !dumb && hyperlinks(env),
        }
    }

    /// `text` as this terminal can show it: unchanged on UTF-8, with every
    /// glyph this CLI draws swapped for ASCII otherwise. Only for our own
    /// words — a job's output is never rewritten.
    pub fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.unicode {
            Cow::Borrowed(text)
        } else {
            ascii(text)
        }
    }

    /// Paint for stdout.
    pub fn out(&self) -> Paint {
        Paint(self.out.color)
    }

    /// Paint for stderr.
    pub fn err(&self) -> Paint {
        Paint(self.err.color)
    }

    /// The spinner's frames.
    pub fn spinner(&self) -> &'static [&'static str] {
        if self.unicode {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        } else {
            &["-", "\\", "|", "/"]
        }
    }

    /// `url`, clickable where the terminal allows it. The text is the URL
    /// itself either way: it is also something to paste to a colleague.
    pub fn link(&self, url: &str, shown: &str) -> String {
        if self.links {
            format!("\x1b]8;;{url}\x1b\\{shown}\x1b]8;;\x1b\\")
        } else {
            shown.to_string()
        }
    }

    /// A deep link as the last line of a command (see `format::deep_link`).
    pub fn link_line(&self, url: &str) -> String {
        let paint = self.err();
        format!(
            "{} {}",
            paint.sgr(DIM, &self.text("→")),
            self.link(url, &paint.sgr(DIM_UNDERLINE, url))
        )
    }
}

/// `LC_ALL`, then `LC_CTYPE`, then `LANG` — the first one set decides, as
/// it does for every other program. Nothing set at all is treated as UTF-8:
/// that is what a desktop terminal with a bare environment almost always
/// is, and the arrows this CLI has always printed never came with a check.
fn locale_is_utf8(env: &dyn Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|key| env(key).filter(|v| !v.is_empty()))
        .is_none_or(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// Terminals known to take OSC 8. `FORCE_HYPERLINK` settles it either way;
/// a multiplexer in between says no unless forced, because older tmux and
/// screen pass the escape through as text.
fn hyperlinks(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if let Some(force) = env("FORCE_HYPERLINK").filter(|v| !v.is_empty()) {
        return force != "0";
    }
    let term = env("TERM").unwrap_or_default();
    if env("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux") {
        return false;
    }
    let program = env("TERM_PROGRAM").unwrap_or_default();
    env("VTE_VERSION")
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v >= 5000)
        || ["iTerm.app", "WezTerm", "vscode", "ghostty"].contains(&program.as_str())
        || env("WT_SESSION").is_some()
        || env("KONSOLE_VERSION").is_some()
        || env("KITTY_WINDOW_ID").is_some()
        || ["kitty", "foot", "alacritty", "ghostty", "wezterm"]
            .iter()
            .any(|t| term.contains(t))
}

/// Every glyph this CLI draws, and what stands in for it without UTF-8.
const ASCII: &[(char, &str)] = &[
    ('✓', "+"),
    ('✗', "x"),
    ('→', "->"),
    ('←', "<-"),
    ('▸', ">"),
    ('│', "|"),
    ('┃', "!"),
    ('─', "-"),
    ('·', "-"),
    ('—', "--"),
    ('–', "-"),
    ('…', "..."),
    ('⚡', "!"),
];

fn ascii(text: &str) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match ASCII.iter().find(|(u, _)| *u == c) {
            Some((_, a)) => out.push_str(a),
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

// -- SGR -------------------------------------------------------------------

pub const BOLD: &str = "1";
pub const DIM: &str = "2";
pub const DIM_UNDERLINE: &str = "2;4";
pub const CYAN: &str = "36";

/// The colours a machine's name can take, in the order they are handed
/// out. Red, green and yellow are missing on purpose: they mean succeeded,
/// failed and no answer, and a machine called `db` should not look like a
/// failure because of the letters in its name.
pub const HUES: [&str; 6] = ["36", "35", "34", "95", "96", "94"];

/// SGR styling for one stream, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint(pub bool);

impl Paint {
    /// `text` wrapped in `\e[{codes}m … \e[0m`, or as it is.
    pub fn sgr(self, codes: &str, text: &str) -> String {
        if self.0 && !codes.is_empty() && !text.is_empty() {
            format!("\x1b[{codes}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// A [`Tone`] from the copy in `format`, as this stream shows it.
    pub fn tone(self, tone: Tone, text: &str) -> String {
        self.sgr(
            match tone {
                Tone::Plain => "",
                Tone::Strong => BOLD,
                Tone::Quiet => DIM,
                Tone::Good => "32",
                Tone::Bad => "31",
                Tone::Unknown => "33",
                Tone::Accent => CYAN,
            },
            text,
        )
    }

    /// A line of toned spans.
    pub fn line(self, spans: &[(Tone, String)]) -> String {
        spans.iter().map(|(t, s)| self.tone(*t, s)).collect()
    }

    // The vocabulary every command draws from. What each means is the
    // point, and it is the same everywhere: green worked, red failed,
    // yellow needs a look, cyan is something to type or in progress, dim
    // is detail — ids, times, labels — that a reader looks up rather than
    // reads. Pad before painting: an SGR sequence has no width on screen
    // but `{:<10}` counts every byte of it.

    /// What the eye should land on: a name, a heading.
    pub fn bold(self, text: impl std::fmt::Display) -> String {
        self.sgr(BOLD, &text.to_string())
    }

    /// Detail: labels, ids, timestamps.
    pub fn dim(self, text: impl std::fmt::Display) -> String {
        self.sgr(DIM, &text.to_string())
    }

    pub fn good(self, text: impl std::fmt::Display) -> String {
        self.tone(Tone::Good, &text.to_string())
    }

    pub fn bad(self, text: impl std::fmt::Display) -> String {
        self.tone(Tone::Bad, &text.to_string())
    }

    pub fn warn(self, text: impl std::fmt::Display) -> String {
        self.tone(Tone::Unknown, &text.to_string())
    }

    /// Something to type, or something happening now.
    pub fn accent(self, text: impl std::fmt::Display) -> String {
        self.tone(Tone::Accent, &text.to_string())
    }

    /// A state word — `succeeded`, `revoked`, `pending` — in the colour of
    /// what it means ([`tone_of`]). Padding inside `text` is kept.
    pub fn state(self, text: impl std::fmt::Display) -> String {
        let text = text.to_string();
        self.tone(tone_of(&text), &text)
    }

    /// A status line that leads with a glyph, the glyph in the colour of
    /// what it says: `✓` green, `✗` red, `!!` and `⏸` yellow, `→` cyan, and
    /// a `·` line — context, not news — dimmed whole. Any `code span` in it
    /// is shown as something to type.
    pub fn status(self, line: &str) -> String {
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let (glyph, rest) = body.split_at(body.find(' ').unwrap_or(body.len()));
        let glyph = match glyph {
            "·" => return format!("{indent}{}", self.dim(body)),
            "✓" => self.good(glyph),
            "✗" => self.bad(glyph),
            "!!" | "!" | "⚠" | "⏸" | "⚡" => self.sgr("1;33", glyph),
            "→" | "▸" => self.accent(glyph),
            "←" => self.dim(glyph),
            _ => return self.prose(line),
        };
        format!("{indent}{glyph}{}", self.prose(rest))
    }

    /// One of our own sentences, with each `code span` shown as something
    /// to type. With colour the backticks go — the colour marks the span —
    /// and without it they stay, because then they are the only mark.
    pub fn prose(self, text: &str) -> String {
        let ticks = text.matches('`').count();
        if !self.0 || ticks < 2 {
            return text.to_string();
        }
        // Paired left to right; an odd one out at the end stays as written.
        let paired = ticks - ticks % 2;
        let mut out = String::with_capacity(text.len() + 16);
        let mut span = String::new();
        let (mut seen, mut code) = (0, false);
        for c in text.chars() {
            if c == '`' && seen < paired {
                seen += 1;
                if code {
                    out.push_str(&self.accent(&span));
                    span.clear();
                }
                code = !code;
            } else if code {
                span.push(c);
            } else {
                out.push(c);
            }
        }
        out
    }
}

/// `error: …` as the last thing a failed command says: the prefix in bold
/// red, and any `command` in the message shown as one.
pub fn error_line(message: &str) -> String {
    let p = err();
    format!("{} {}", p.sgr("1;31", "error:"), p.prose(message))
}

/// `warning: …`, the same way, in yellow.
pub fn warning_line(message: &str) -> String {
    let p = err();
    format!("{} {}", p.sgr("1;33", "warning:"), p.prose(message))
}

/// `--color` as written on the command line, for clap: its help and its
/// errors are rendered while the arguments are still being parsed, before
/// [`set_choice`] can have run.
pub fn choice_from_args() -> ColorChoice {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy().into_owned();
        let value = match arg.strip_prefix("--color") {
            Some("") => args.next().map(|v| v.to_string_lossy().into_owned()),
            Some(rest) => rest.strip_prefix('=').map(str::to_string),
            None if arg == "--" => break,
            None => continue,
        };
        return match value.as_deref() {
            Some("always") => ColorChoice::Always,
            Some("never") => ColorChoice::Never,
            _ => ColorChoice::Auto,
        };
    }
    ColorChoice::Auto
}

/// A label in a report on stdout — `state`, `node:` — drawn as detail so
/// the value beside it is what reads.
pub fn label(text: &str) -> String {
    out().dim(text)
}

/// [`label`], padded to `width` columns first — the colour has no width on
/// screen, and `{:<10}` around it would count it.
pub fn label_w(text: &str, width: usize) -> String {
    out().dim(format!("{text:<width$}"))
}

/// A command to type, in a report on stdout.
pub fn cmd(text: &str) -> String {
    out().accent(text)
}

static CAPS: OnceLock<Caps> = OnceLock::new();

/// The terminal, read once for the process — after `--color` is known.
pub fn caps() -> Caps {
    *CAPS.get_or_init(Caps::detect)
}

/// Paint for stdout: tables, reports, the answer.
pub fn out() -> Paint {
    caps().out()
}

/// Paint for stderr: status, hints, errors.
pub fn err() -> Paint {
    caps().err()
}

/// What a state word means, by the worst word in it: `holder revoked` is
/// revoked, `ok (stale)` is stale. A word nobody listed is plain — a
/// colour the reader cannot explain is worse than none.
pub fn tone_of(state: &str) -> Tone {
    let s = state.trim().to_ascii_lowercase();
    if s.is_empty() || s == "-" {
        return Tone::Quiet;
    }
    const BAD: &[&str] = &[
        "failed",
        "fail",
        "failure",
        "error",
        "errors",
        "dead",
        "revoked",
        "denied",
        "refused",
        "unhealthy",
        "expired",
        "rejected",
        "crashed",
        "lost",
        "quarantined",
        "broken",
        "invalid",
        "unreachable",
        "down",
        "corrupt",
        "mismatch",
        "missing",
        "abandoned",
    ];
    const WARN: &[&str] = &[
        "pending",
        "queued",
        "waiting",
        "suspect",
        "degraded",
        "draining",
        "stale",
        "warn",
        "warning",
        "expiring",
        "paused",
        "held",
        "timed",
        "timeout",
        "partial",
        "behind",
        "unprotected",
        "retrying",
        "requested",
        "unknown",
        "outdated",
        "blocked",
        "throttled",
    ];
    const ACTIVE: &[&str] = &[
        "running",
        "starting",
        "deploying",
        "rolling",
        "syncing",
        "pulling",
        "building",
        "restarting",
        "stopping",
        "rolling-out",
        "in-progress",
    ];
    const GOOD: &[&str] = &[
        "succeeded",
        "success",
        "ok",
        "active",
        "ready",
        "alive",
        "healthy",
        "present",
        "enforced",
        "valid",
        "applied",
        "pass",
        "passed",
        "available",
        "enabled",
        "founder",
        "up",
        "done",
        "complete",
        "completed",
        "published",
        "adopted",
        "approved",
        "online",
        "protected",
        "verified",
        "current",
        "synced",
        "found",
        "reachable",
        "yes",
        "live",
        "serving",
        "unlocked",
        "trusted",
    ];
    const QUIET: &[&str] = &[
        "cancelled",
        "canceled",
        "stopped",
        "disabled",
        "none",
        "off",
        "retired",
        "removed",
        "skipped",
        "idle",
        "absent",
        "left",
        "superseded",
        "no",
        "inactive",
        "unused",
        "n/a",
    ];
    let mut best: Option<(u8, Tone)> = None;
    for word in s.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '/')) {
        let rank = if BAD.contains(&word) {
            (5, Tone::Bad)
        } else if WARN.contains(&word) {
            (4, Tone::Unknown)
        } else if ACTIVE.contains(&word) {
            (3, Tone::Accent)
        } else if GOOD.contains(&word) {
            (2, Tone::Good)
        } else if QUIET.contains(&word) {
            (1, Tone::Quiet)
        } else {
            continue;
        };
        if best.is_none_or(|(r, _)| rank.0 > r) {
            best = Some(rank);
        }
    }
    best.map_or(Tone::Plain, |(_, t)| t)
}

/// What a status glyph means, if it is one.
pub fn glyph_tone(glyph: &str) -> Option<Tone> {
    Some(match glyph.trim() {
        "●" | "✓" | "+" => Tone::Good,
        "◐" | "◑" | "!" | "⚠" | "~" | "?" => Tone::Unknown,
        "○" | "✗" | "⊘" | "✕" | "×" => Tone::Bad,
        "·" | "–" => Tone::Quiet,
        _ => return None,
    })
}

/// A mesh id — `job-…`, `mw1…`, `grant-…` — which a reader copies rather
/// than reads, and so gets drawn as detail.
pub fn looks_like_id(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "mw1", "mwu1", "job-", "fan-", "run-", "grant-", "gang-", "aud-", "apr-", "inv-", "lease-",
        "mesh-", "upg-", "act-", "bndl-", "obj-",
    ];
    let t = text.trim();
    PREFIXES.iter().any(|p| {
        t.strip_prefix(p).is_some_and(|rest| {
            rest.len() >= 6 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '…')
        })
    })
}

// -- size and width --------------------------------------------------------

/// stderr's size as (columns, rows), when it is a terminal that says.
pub fn size() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        let ws = rustix::termios::tcgetwinsize(std::io::stderr()).ok()?;
        (ws.ws_col > 0 && ws.ws_row > 0).then_some((usize::from(ws.ws_col), usize::from(ws.ws_row)))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Columns `text` takes on screen. For plain text only: an SGR sequence
/// would be counted as the letters it is made of.
pub fn width(text: &str) -> usize {
    text.width()
}

/// `text` cut to fit `cols` columns, ending in `…` (or `...`) when cut.
pub fn clip(text: &str, cols: usize, unicode: bool) -> String {
    if width(text) <= cols {
        return text.to_string();
    }
    let ellipsis = if unicode { "…" } else { "..." };
    let room = cols.saturating_sub(width(ellipsis));
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(ellipsis);
    out
}

/// A line of a job's output made safe to draw inside the live board: what
/// a progress bar left after its last `\r`, with escape sequences and
/// control characters taken out. The board redraws itself by counting
/// lines, and a stray cursor movement or colour from a job would break the
/// count or bleed into the next row.
pub fn plain(line: &str) -> String {
    let line = line
        .rsplit('\r')
        .find(|s| !s.trim().is_empty())
        .unwrap_or("");
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn caps(vars: &[(&str, &str)], out_tty: bool, err_tty: bool, choice: ColorChoice) -> Caps {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Caps::decide(&|k| vars.get(k).cloned(), out_tty, err_tty, choice)
    }

    #[test]
    fn colour_is_decided_per_stream() {
        // `> out.txt`: the file gets plain bytes, the screen gets colour.
        let c = caps(
            &[("TERM", "xterm-256color")],
            false,
            true,
            ColorChoice::Auto,
        );
        assert!(!c.out.color);
        assert!(c.err.color);
        assert!(c.live);
    }

    #[test]
    fn no_color_turns_colour_off_but_keeps_the_board() {
        let c = caps(
            &[("TERM", "xterm-256color"), ("NO_COLOR", "1")],
            true,
            true,
            ColorChoice::Auto,
        );
        assert!(!c.out.color && !c.err.color);
        assert!(c.live, "NO_COLOR is about colour, not about redrawing");
        // An empty NO_COLOR is unset, per no-color.org.
        let c = caps(&[("NO_COLOR", "")], true, true, ColorChoice::Auto);
        assert!(c.err.color);
    }

    #[test]
    fn clicolor_force_colours_a_pipe_and_the_flag_beats_everything() {
        let c = caps(&[("CLICOLOR_FORCE", "1")], false, false, ColorChoice::Auto);
        assert!(c.out.color);
        let c = caps(&[("NO_COLOR", "1")], true, true, ColorChoice::Always);
        assert!(c.out.color);
        let c = caps(&[("CLICOLOR_FORCE", "1")], true, true, ColorChoice::Never);
        assert!(!c.err.color);
    }

    #[test]
    fn a_dumb_terminal_gets_plain_ascii_and_no_board() {
        let c = caps(&[("TERM", "dumb")], true, true, ColorChoice::Auto);
        assert!(!c.err.color);
        assert!(!c.live);
        assert!(!c.unicode);
        assert_eq!(c.text("✓ IVY · 0.4s — ok…"), "+ IVY - 0.4s -- ok...");
    }

    #[test]
    fn the_locale_decides_the_glyphs() {
        for (lang, utf8) in [
            ("en_GB.UTF-8", true),
            ("C.utf8", true),
            ("C", false),
            ("POSIX", false),
        ] {
            let c = caps(&[("LANG", lang)], true, true, ColorChoice::Auto);
            assert_eq!(c.unicode, utf8, "{lang}");
        }
        // LC_ALL outranks LANG.
        let c = caps(
            &[("LANG", "en_GB.UTF-8"), ("LC_ALL", "C")],
            true,
            true,
            ColorChoice::Auto,
        );
        assert!(!c.unicode);
        // Nothing set: assume the desktop terminal it almost always is.
        assert!(caps(&[], true, true, ColorChoice::Auto).unicode);
    }

    #[test]
    fn links_only_where_the_terminal_is_known_to_take_them() {
        assert!(caps(&[("VTE_VERSION", "7800")], true, true, ColorChoice::Auto).links);
        assert!(!caps(&[("TERM", "xterm")], true, true, ColorChoice::Auto).links);
        assert!(
            !caps(
                &[("VTE_VERSION", "7800"), ("TMUX", "/tmp/tmux")],
                true,
                true,
                ColorChoice::Auto
            )
            .links
        );
        assert!(!caps(&[("VTE_VERSION", "7800")], true, false, ColorChoice::Auto).links);
        assert!(caps(&[("FORCE_HYPERLINK", "1")], true, true, ColorChoice::Auto).links);
    }

    #[test]
    fn a_state_reads_as_its_worst_word() {
        for (word, tone) in [
            ("succeeded", Tone::Good),
            ("active", Tone::Good),
            ("revoked", Tone::Bad),
            ("holder revoked", Tone::Bad),
            ("ok (stale)", Tone::Unknown),
            ("running", Tone::Accent),
            ("cancelled", Tone::Quiet),
            ("-", Tone::Quiet),
            ("  pending  ", Tone::Unknown),
            // A word nobody listed gets no colour it cannot explain.
            ("frobnicated", Tone::Plain),
        ] {
            assert_eq!(tone_of(word), tone, "{word:?}");
        }
    }

    #[test]
    fn glyphs_and_ids_are_recognised() {
        assert_eq!(glyph_tone("●"), Some(Tone::Good));
        assert_eq!(glyph_tone("⊘"), Some(Tone::Bad));
        assert_eq!(glyph_tone("◐"), Some(Tone::Unknown));
        assert_eq!(glyph_tone("IVY"), None);
        assert!(looks_like_id("job-agqlmpyfdq"));
        assert!(looks_like_id("mw1i3cx26ynsoib"));
        assert!(looks_like_id("fan-agqlmpyfdrzp2gw…"));
        assert!(!looks_like_id("job-queue"), "too short to be an id");
        assert!(!looks_like_id("merope"));
    }

    #[test]
    fn prose_colours_code_and_keeps_the_rest() {
        let on = Paint(true);
        assert_eq!(
            on.prose("run `meshnode up` now"),
            "run \x1b[36mmeshnode up\x1b[0m now"
        );
        // An odd backtick out is left as written.
        assert_eq!(
            on.prose("`a` and `b` and ` c"),
            "\x1b[36ma\x1b[0m and \x1b[36mb\x1b[0m and ` c"
        );
        // Without colour the backticks are the only mark, so they stay.
        assert_eq!(Paint(false).prose("run `meshnode up`"), "run `meshnode up`");
    }

    #[test]
    fn a_status_line_is_coloured_by_its_glyph() {
        let on = Paint(true);
        assert_eq!(on.status("✓ done"), "\x1b[32m✓\x1b[0m done");
        assert_eq!(on.status("  ✗ broke"), "  \x1b[31m✗\x1b[0m broke");
        assert_eq!(on.status("· context"), "\x1b[2m· context\x1b[0m");
        assert_eq!(on.status("no glyph `here`"), "no glyph \x1b[36mhere\x1b[0m");
        assert_eq!(Paint(false).status("✓ done"), "✓ done");
    }

    #[test]
    fn paint_off_is_the_text_itself() {
        assert_eq!(Paint(false).sgr(BOLD, "IVY"), "IVY");
        assert_eq!(Paint(true).sgr(BOLD, "IVY"), "\x1b[1mIVY\x1b[0m");
        assert_eq!(Paint(true).sgr(BOLD, ""), "");
    }

    #[test]
    fn a_board_line_loses_what_would_break_the_board() {
        assert_eq!(plain("\x1b[32mok\x1b[0m done"), "ok done");
        assert_eq!(plain(" 10%\r 50%\r100%"), "100%");
        assert_eq!(plain("a\tb"), "a b");
        assert_eq!(plain("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
        assert_eq!(plain("bell\x07"), "bell");
    }

    #[test]
    fn clipping_counts_columns_not_bytes() {
        assert_eq!(clip("0123456789", 6, true), "01234…");
        assert_eq!(clip("0123456789", 6, false), "012...");
        assert_eq!(clip("short", 6, true), "short");
        // Two columns per CJK character.
        assert_eq!(clip("日本語のテキスト", 7, true), "日本語…");
    }
}
