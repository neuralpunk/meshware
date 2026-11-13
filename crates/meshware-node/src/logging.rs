//! Daemon logging (MASTERPLAN §12): tracing → stderr by default, or a
//! size-rotated file when `[log] dir` is set; `text` or `json` format; the
//! level filter is swappable live so SIGHUP config reloads apply it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mesh_core::config::NodeConfig;
use tracing_subscriber::{EnvFilter, fmt::MakeWriter, reload};

const LOG_FILE: &str = "meshnode.log";

/// Handle to retune the level filter after init (SIGHUP).
#[derive(Clone)]
pub struct LogControl {
    handle: reload::Handle<EnvFilter, tracing_subscriber::Registry>,
}

impl LogControl {
    pub fn set_level(&self, level: &str) {
        let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(level));
        if let Err(e) = self.handle.reload(filter) {
            eprintln!("meshnode: cannot reload log filter: {e}");
        }
    }
}

/// A size-capped rolling file writer: `meshnode.log` rotates to
/// `meshnode.log.1` … `.N` once it exceeds the cap (MASTERPLAN §12 —
/// built-in rotation, no logrotate required).
struct RollingFile {
    dir: PathBuf,
    file: std::fs::File,
    size: u64,
    max_size: u64,
    max_files: u32,
}

impl RollingFile {
    fn open(dir: &Path, max_size: u64, max_files: u32) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOG_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RollingFile {
            dir: dir.to_path_buf(),
            file,
            size,
            max_size,
            max_files,
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let base = self.dir.join(LOG_FILE);
        // Shift meshnode.log.(N-1) → .N, …, meshnode.log → .1.
        let _ = std::fs::remove_file(self.dir.join(format!("{LOG_FILE}.{}", self.max_files)));
        for i in (1..self.max_files).rev() {
            let from = self.dir.join(format!("{LOG_FILE}.{i}"));
            if from.exists() {
                let _ = std::fs::rename(&from, self.dir.join(format!("{LOG_FILE}.{}", i + 1)));
            }
        }
        let _ = std::fs::rename(&base, self.dir.join(format!("{LOG_FILE}.1")));
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)?;
        self.size = 0;
        Ok(())
    }
}

/// Cloneable writer handle for tracing's `MakeWriter`.
#[derive(Clone)]
pub struct RollingWriter(Arc<Mutex<RollingFile>>);

impl Write for RollingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.0.lock().expect("log writer lock");
        if inner.size + buf.len() as u64 > inner.max_size {
            let _ = inner.rotate();
        }
        let n = inner.file.write(buf)?;
        inner.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().expect("log writer lock").file.flush()
    }
}

impl<'a> MakeWriter<'a> for RollingWriter {
    type Writer = RollingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Initialize tracing per config. Returns the live level control.
pub fn init(config: &NodeConfig) -> LogControl {
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.clone()));
    let (filter_layer, handle) = reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter_layer);
    let control = LogControl { handle };

    let file_writer = config.log.dir.as_ref().and_then(|dir| {
        match RollingFile::open(dir, config.log.max_size_bytes, config.log.max_files) {
            Ok(f) => Some(RollingWriter(Arc::new(Mutex::new(f)))),
            Err(e) => {
                eprintln!(
                    "meshnode: cannot open log dir {} ({e}); logging to stderr",
                    dir.display()
                );
                None
            }
        }
    });

    // Under systemd, with stderr already wired into the journal
    // (`JOURNAL_STREAM` is how systemd says so), write structured
    // records instead of text lines: `journalctl -u meshnode -o json`
    // then carries `JOB`, `TRACE`, `GROUP` as fields to filter on rather
    // than words to grep for. A `[log] dir` still wins — an operator who
    // wrote one down wants a file.
    if file_writer.is_none()
        && std::env::var_os("JOURNAL_STREAM").is_some()
        && let Some(journal) = journald_layer()
    {
        registry.with(journal).init();
        return control;
    }

    match (file_writer, config.log.json) {
        (Some(w), true) => registry
            .with(fmt::layer().json().with_writer(w).with_ansi(false))
            .init(),
        (Some(w), false) => registry
            .with(fmt::layer().with_writer(w).with_ansi(false))
            .init(),
        (None, true) => registry
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .init(),
        (None, false) => registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .init(),
    }
    control
}

#[cfg(target_os = "linux")]
fn journald_layer() -> Option<tracing_journald::Layer> {
    match tracing_journald::layer() {
        Ok(layer) => Some(
            layer
                .with_syslog_identifier("meshnode".into())
                // **Field names as written, not `F_`-prefixed** (ENG-21).
                // The default prefixes every field, so the span field
                // `job` reaches the journal as `F_JOB` — which is what an
                // operator's `jq 'select(.JOB==…)'` silently does not
                // match, and a documented field name that does not exist
                // is the same class of problem as a counter that goes
                // down. Checked against journald's well-known names
                // (`MESSAGE`, `PRIORITY`, `CODE_*`, `SYSLOG_*`): nothing
                // this daemon logs collides with one.
                .with_field_prefix(None),
        ),
        Err(e) => {
            eprintln!("meshnode: journald is not reachable ({e}); logging to stderr");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn journald_layer() -> Option<tracing_subscriber::fmt::Layer<tracing_subscriber::Registry>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_file_rotates_and_caps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = RollingFile::open(dir.path(), 128, 2).expect("open");
        let mut w = RollingWriter(Arc::new(Mutex::new(file)));
        for i in 0..40 {
            writeln!(w, "line {i}: padding padding padding").expect("write");
        }
        w.flush().expect("flush");
        assert!(dir.path().join(LOG_FILE).exists());
        assert!(dir.path().join(format!("{LOG_FILE}.1")).exists());
        assert!(dir.path().join(format!("{LOG_FILE}.2")).exists());
        // max_files = 2: no .3 may ever appear.
        assert!(!dir.path().join(format!("{LOG_FILE}.3")).exists());
        // The live file respects the cap (plus at most one line of slack).
        let live = std::fs::metadata(dir.path().join(LOG_FILE)).expect("meta");
        assert!(live.len() <= 128 + 64);
    }
}
