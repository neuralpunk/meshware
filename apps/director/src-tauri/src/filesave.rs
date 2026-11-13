//! One document, written to a file the person chose, in pieces.
//!
//! `snapshot_save` and `bench_save_report` take the whole document as a
//! `String` argument and write it in one call, and for a PNG or a report
//! that is the right shape. An answer set is not that shape. Six hundred
//! machines each returning a megabyte of stdout is a JSON document larger
//! than the webview will hold as one string, and marshalling it across the
//! IPC bridge as a single argument means it exists three times at once —
//! as the JavaScript string, as the serialised message, and as the `String`
//! this side deserialises.
//!
//! So the frontend opens a sink, writes a megabyte at a time, and closes
//! it. The document never exists whole anywhere: only the piece in flight
//! and the bytes already on disk. Each write also gives the event loop a
//! turn, which is what keeps the progress bar moving and Esc working while
//! a gigabyte goes out.
//!
//! **A cancelled or failed save leaves nothing behind — and takes nothing
//! with it** (DIR-10). The bytes go to a hidden file beside the one chosen
//! ([`Staged`]), which becomes that file only when the save finishes, in
//! one rename. `file_save_abort`, and a write that fails halfway, remove
//! the staged file and nothing else. Half an export that looks like a whole
//! one is the failure mode worth writing around — it is the same reason
//! `parallel_save_outputs` refuses a short transfer rather than filing it.
//!
//! It used to create the chosen file at once — truncating it, if the person
//! was replacing one — and delete it on a cancel. So pressing Esc during an
//! export destroyed the file being replaced, before and after alike.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use tauri::AppHandle;

/// A file being written beside the one it will replace (DIR-10).
///
/// The target is not touched until [`Staged::commit`], and then only by a
/// rename, so it is at every moment either what it was or the whole new
/// document — never empty, never half.
pub(crate) struct Staged {
    /// Where it is going: the chosen file, through any symlink, so a link
    /// is written through to its file as a plain write would, rather than
    /// being replaced by one.
    target: PathBuf,
    /// Where it is being written: a hidden sibling, on the same filesystem,
    /// which is what makes the rename atomic.
    temp: PathBuf,
    file: BufWriter<File>,
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

impl Staged {
    pub(crate) fn create(chosen: &Path) -> Result<Staged, String> {
        let target = if chosen.is_symlink() {
            std::fs::canonicalize(chosen)
                .map_err(|e| format!("cannot follow {}: {e}", chosen.display()))?
        } else {
            chosen.to_path_buf()
        };
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} is not a file name", target.display()))?;
        let temp = target.with_file_name(format!(
            ".{name}.{}-{}.part",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("cannot write beside {}: {e}", target.display()))?;
        Ok(Staged {
            target,
            temp,
            file: BufWriter::with_capacity(1 << 20, file),
        })
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.file
            .write_all(bytes)
            .map_err(|e| format!("cannot write {}: {e}", self.target.display()))
    }

    /// Make it the chosen file: flushed, synced, given the permissions of
    /// the file it replaces, renamed into place. On any failure the staged
    /// file is removed and the target is as it was.
    pub(crate) fn commit(self) -> Result<PathBuf, String> {
        let Staged { target, temp, file } = self;
        let result = (|| {
            let file = file.into_inner().map_err(|e| e.error().to_string())?;
            // The bytes are the whole point of the button, so they go to
            // the platter before it says "saved".
            file.sync_all().map_err(|e| e.to_string())?;
            // Replacing a file keeps its mode, as truncating it did; a new
            // file gets this process's (private) default.
            if let Ok(meta) = std::fs::metadata(&target) {
                std::fs::set_permissions(&temp, meta.permissions()).map_err(|e| e.to_string())?;
            }
            drop(file);
            std::fs::rename(&temp, &target).map_err(|e| e.to_string())?;
            // …and the rename itself, which lives in the directory.
            if let Some(dir) = target.parent()
                && let Ok(dir) = File::open(dir)
            {
                let _ = dir.sync_all();
            }
            Ok::<_, String>(())
        })();
        match result {
            Ok(()) => Ok(target),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(format!("cannot write {}: {e}", target.display()))
            }
        }
    }

    /// Give up: the staged file goes, the target stays as it was.
    pub(crate) fn abandon(self) {
        let temp = self.temp.clone();
        drop(self);
        let _ = std::fs::remove_file(temp);
    }
}

/// Write a whole document over `chosen` the same way (DIR-10): the target
/// is either what it was or all of `bytes`, never a truncated half.
pub(crate) fn write_replacing(chosen: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let mut staged = Staged::create(chosen)?;
    if let Err(e) = staged.write_all(bytes) {
        staged.abandon();
        return Err(e);
    }
    staged.commit()
}

/// The file to write, from what the save dialog returned (DIR-10).
///
/// A person who typed "answers" into the name box should still get a file
/// the next program recognises, so a missing extension is added — but only
/// when that names nothing yet. The dialog asked whether to replace
/// `answers`, if it existed; it never asked about `answers.json`, and
/// replacing that one would be writing over a file nobody agreed to.
pub(crate) fn with_default_extension(chosen: PathBuf, ext: Option<&str>) -> PathBuf {
    match (chosen.extension(), ext) {
        (None, Some(ext)) => {
            let with = chosen.with_extension(ext);
            if with.exists() || with.is_symlink() {
                chosen
            } else {
                with
            }
        }
        _ => chosen,
    }
}

/// A file being written, and where it is going.
struct Sink {
    staged: Staged,
    written: u64,
}

/// How many saves may be in flight at once.
///
/// One is the honest number — a person picks a file and watches it fill —
/// and the cap exists so that a frontend which forgets to close its sinks
/// runs out of ids rather than out of file descriptors.
const MAX_OPEN: usize = 4;

/// The largest single chunk a sink accepts (16 MiB).
///
/// The caller's own batching is an order of magnitude under this; the
/// limit is here so that a bug on the other side of the bridge cannot ask
/// this process to materialise an arbitrarily large `String`.
const MAX_CHUNK: usize = 16 * 1024 * 1024;

static SINKS: OnceLock<Mutex<HashMap<u64, Sink>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The open sinks. A poisoned lock is recovered rather than propagated: the
/// map is a bag of file handles, and one panicking writer says nothing
/// about whether the *other* saves are still good.
fn sinks() -> MutexGuard<'static, HashMap<u64, Sink>> {
    SINKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Close a sink and remove what it had written — its staged file, never
/// the file it would have replaced.
fn discard(open: &mut HashMap<u64, Sink>, id: u64) {
    if let Some(sink) = open.remove(&id) {
        sink.staged.abandon();
    }
}

/// Ask where to save, create the file, and answer with a handle to write
/// into. `None` is a cancel, which is not an error.
#[tauri::command]
pub async fn file_save_begin(
    app: AppHandle,
    title: String,
    suggested: String,
    filter_name: Option<String>,
    extensions: Option<Vec<String>>,
) -> Result<Option<u64>, String> {
    let exts: Vec<&str> = extensions.iter().flatten().map(String::as_str).collect();
    let filter = filter_name.as_deref().map(|name| (name, exts.as_slice()));
    let Some(path) = crate::ask_where_to_save(&app, &title, &suggested, filter).await? else {
        return Ok(None);
    };
    let path = with_default_extension(
        path,
        extensions
            .as_ref()
            .and_then(|e| e.first())
            .map(String::as_str),
    );
    open_sink(&path).map(Some)
}

/// Register a sink writing toward `path`. Split from the command so it can
/// be driven without a dialog.
fn open_sink(path: &Path) -> Result<u64, String> {
    let mut open = sinks();
    if open.len() >= MAX_OPEN {
        return Err("too many saves are already in flight".into());
    }
    let staged = Staged::create(path)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    open.insert(id, Sink { staged, written: 0 });
    Ok(id)
}

/// Append a chunk. Answers with the running total, which is what the
/// progress bar is drawn from.
#[tauri::command]
pub async fn file_save_write(id: u64, chunk: String) -> Result<u64, String> {
    if chunk.len() > MAX_CHUNK {
        return Err(format!(
            "a {} byte chunk is larger than a save sink accepts",
            chunk.len()
        ));
    }
    // On the blocking pool, not on an async worker: a slow disk must cost a
    // thread that exists for exactly this and never one of the handful that
    // schedule everything Director asks its own engine (see `onePicker` in
    // `api.ts` for what that failure looks like from the outside).
    tauri::async_runtime::spawn_blocking(move || {
        let mut open = sinks();
        let Some(sink) = open.get_mut(&id) else {
            return Err("that save is no longer open".to_string());
        };
        match sink.staged.write_all(chunk.as_bytes()) {
            Ok(()) => {
                sink.written += chunk.len() as u64;
                Ok(sink.written)
            }
            Err(message) => {
                discard(&mut open, id);
                Err(message)
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Flush, sync, move into place, and answer with the path written.
#[tauri::command]
pub async fn file_save_finish(id: u64) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || finish_sink(id))
        .await
        .map_err(|e| e.to_string())?
}

fn finish_sink(id: u64) -> Result<String, String> {
    let Some(sink) = sinks().remove(&id) else {
        return Err("that save is no longer open".to_string());
    };
    sink.staged
        .commit()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Give up: close the sink and remove the partial file.
#[tauri::command]
pub async fn file_save_abort(id: u64) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut open = sinks();
        discard(&mut open, id);
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mw-filesave-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Nothing but the named files: no staged `.part` left behind.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The bug: Esc during an export over an existing file deleted it.
    #[test]
    fn a_cancelled_save_leaves_the_file_it_would_have_replaced() {
        let dir = scratch("cancel");
        let path = dir.join("answers.json");
        std::fs::write(&path, "the old answers").expect("seed");

        let id = open_sink(&path).expect("open");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the old answers"
        );
        sinks()
            .get_mut(&id)
            .expect("sink")
            .staged
            .write_all(b"half of the new")
            .expect("write");
        discard(&mut sinks(), id);

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the old answers"
        );
        assert_eq!(names(&dir), ["answers.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_save_replaces_the_file_whole_and_keeps_its_mode() {
        let dir = scratch("finish");
        let path = dir.join("answers.json");
        std::fs::write(&path, "the old answers").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let id = open_sink(&path).expect("open");
        sinks()
            .get_mut(&id)
            .expect("sink")
            .staged
            .write_all(b"the new answers")
            .expect("write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the old answers"
        );
        let written = finish_sink(id).expect("finish");

        assert_eq!(written, path.to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "the new answers"
        );
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert_eq!(names(&dir), ["answers.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_is_written_through_to_its_file() {
        let dir = scratch("link");
        let real = dir.join("real.png");
        std::fs::write(&real, "old").expect("seed");
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&real, &link).expect("link");

        write_replacing(&link, b"new").expect("write");
        assert!(link.is_symlink(), "the link is still a link");
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_added_extension_never_lands_on_a_file_nobody_was_asked_about() {
        let dir = scratch("ext");
        // Nothing there: "answers" becomes answers.json.
        assert_eq!(
            with_default_extension(dir.join("answers"), Some("json")),
            dir.join("answers.json")
        );
        // answers.json exists and the dialog only asked about "answers":
        // write what was asked about.
        std::fs::write(dir.join("answers.json"), "somebody else's").expect("seed");
        assert_eq!(
            with_default_extension(dir.join("answers"), Some("json")),
            dir.join("answers")
        );
        // A name with an extension is left alone.
        assert_eq!(
            with_default_extension(dir.join("answers.txt"), Some("json")),
            dir.join("answers.txt")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
