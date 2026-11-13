//! Sparklines for `meshnode top --follow`: a few minutes of history per
//! row, drawn in eight block glyphs, so a number reads as a direction.
//!
//! Kept by the CLI, never by the daemon. Telemetry already arrives every
//! gossip round; remembering the last few readings is a client-side
//! concern, and a node should not grow a time series for every peer on
//! the off chance somebody is watching.

use std::collections::{HashMap, VecDeque};

/// Columns a sparkline occupies.
pub const WIDTH: usize = 16;

/// Readings kept per key. At a two-second interval this is five minutes,
/// which is what "is it ramping or idling down" needs and no more.
pub const KEEP: usize = 150;

const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Per-key history, in the order readings arrived.
#[derive(Debug, Default)]
pub struct History {
    series: HashMap<String, VecDeque<f64>>,
    samples: usize,
}

impl History {
    /// Record one reading (0–100) for `key`. A `None` reading is a gap and
    /// is recorded as one, so a node that stopped reporting shows a hole
    /// rather than a flat line.
    pub fn push(&mut self, key: &str, value: Option<f64>) {
        let s = self.series.entry(key.to_string()).or_default();
        s.push_back(value.unwrap_or(f64::NAN));
        while s.len() > KEEP {
            s.pop_front();
        }
    }

    /// One tick of readings has been taken; remembered so the header can
    /// say how much time the line covers.
    pub fn tick(&mut self) {
        self.samples = (self.samples + 1).min(KEEP);
    }

    /// How many readings the widest line covers.
    pub fn samples(&self) -> usize {
        self.samples
    }

    /// The line for `key`: [`WIDTH`] glyphs, the newest reading at the
    /// right. Fewer readings than columns pad on the left with spaces, so a
    /// line grows in from the right as history accrues; more readings than
    /// columns are bucketed by their mean.
    pub fn line(&self, key: &str) -> String {
        let Some(s) = self.series.get(key) else {
            return " ".repeat(WIDTH);
        };
        render(s.iter().copied())
    }
}

/// Render an iterator of readings (0–100, NaN = gap) into a sparkline.
pub fn render(readings: impl IntoIterator<Item = f64>) -> String {
    let values: Vec<f64> = readings.into_iter().collect();
    if values.is_empty() {
        return " ".repeat(WIDTH);
    }
    let glyph = |v: f64| -> char {
        if v.is_nan() {
            return '·';
        }
        let idx = ((v.clamp(0.0, 100.0) / 100.0) * 7.0).round() as usize;
        GLYPHS[idx.min(7)]
    };
    if values.len() <= WIDTH {
        let mut out = " ".repeat(WIDTH - values.len());
        out.extend(values.iter().map(|v| glyph(*v)));
        return out;
    }
    // Bucket into WIDTH columns, each the mean of its slice of readings
    // (gaps excluded; an all-gap bucket is a gap).
    let per = values.len() as f64 / WIDTH as f64;
    (0..WIDTH)
        .map(|i| {
            let lo = (i as f64 * per).floor() as usize;
            let hi = (((i + 1) as f64 * per).floor() as usize).max(lo + 1);
            let slice = &values[lo..hi.min(values.len())];
            let real: Vec<f64> = slice.iter().copied().filter(|v| !v.is_nan()).collect();
            if real.is_empty() {
                '·'
            } else {
                glyph(real.iter().sum::<f64>() / real.len() as f64)
            }
        })
        .collect()
}

/// A single-reading bar for a table cell: `▇▇▇▇▇▇▁▁▁▁ 60%`.
pub fn bar(pct: Option<f64>, width: usize) -> String {
    match pct {
        None => format!("{:<width$}   -", "", width = width),
        Some(p) => {
            let filled = ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
            let mut s = String::with_capacity(width + 6);
            for i in 0..width {
                s.push(if i < filled { '▇' } else { '▁' });
            }
            s.push_str(&format!(" {p:>3.0}%"));
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_history_grows_in_from_the_right() {
        let line = render([0.0, 50.0, 100.0]);
        assert_eq!(line.chars().count(), WIDTH);
        assert!(line.starts_with("             "));
        assert!(line.ends_with("▁▅█"), "{line:?}");
    }

    #[test]
    fn a_long_history_is_bucketed_to_width() {
        let readings: Vec<f64> = (0..KEEP).map(|i| (i % 100) as f64).collect();
        assert_eq!(render(readings).chars().count(), WIDTH);
    }

    #[test]
    fn a_gap_is_a_dot_not_a_zero() {
        assert!(render([f64::NAN, 100.0]).ends_with("·█"));
    }

    #[test]
    fn a_bar_is_as_wide_as_asked() {
        assert_eq!(bar(Some(50.0), 10), "▇▇▇▇▇▁▁▁▁▁  50%");
        assert_eq!(bar(None, 4), "       -");
    }
}
