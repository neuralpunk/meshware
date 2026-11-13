// Write a document to a file the person picks, in pieces.
//
// The one-shot form — build the whole string, hand it to a command — is
// right up to about a megabyte and wrong after that. Past that the string
// exists three times over (the JavaScript value, the serialised IPC
// message, the `String` Rust deserialises), the bridge blocks the main
// thread for as long as the copy takes, and there is nothing to draw a
// progress bar from. An answer set from six hundred machines is routinely
// past that; the reason this file exists is that it should not matter how
// far past.
//
// So: `filesave.rs` opens a sink and this walks a generator into it. The
// document is only ever a chunk in flight plus the bytes already on disk,
// each `await` gives the event loop a turn (the window keeps painting,
// Cancel keeps working), and a save that fails or is cancelled removes its
// own partial file rather than leaving something that looks finished.
import { api } from "./api";

/** How much text goes over the bridge at once. */
const CHUNK = 1 << 20;

export type StreamedSave = {
  /** Shown as the chooser's title. */
  title: string;
  /** The filename offered in the chooser. Rust sanitises it. */
  suggested: string;
  /** Added if the person typed a name without one, e.g. `"json"`. */
  extension?: string;
  /** The chooser's file-type filter, e.g. `"JSON document"`. */
  filterName?: string;
  /** The document, in as many pieces as it likes. Called once. */
  chunks: () => Iterable<string> | AsyncIterable<string>;
  /** Bytes on disk so far, after each write. */
  onProgress?: (bytes: number) => void;
  /** Polled between chunks; `true` abandons the save and unlinks the file. */
  cancelled?: () => boolean;
};

/**
 * Ask where to save, then stream it there.
 *
 * Resolves to the path written, or `null` if the person cancelled — either
 * at the chooser or part-way through. Throws only when the write itself
 * failed, and the partial file is gone by then.
 */
export async function saveStreamed(save: StreamedSave): Promise<string | null> {
  const id = await api.fileSaveBegin(
    save.title,
    save.suggested,
    save.filterName ?? null,
    save.extension ? [save.extension] : null,
  );
  if (id === null) return null;

  // Chunks are accumulated to `CHUNK` rather than sent as the generator
  // yields them: a per-line round-trip over the bridge is millions of
  // messages for one file, and the bridge — not the disk — is what would
  // then set how long a save takes.
  //
  // …and split back down to `CHUNK` on the way out, because a generator is
  // free to yield a piece far larger than that. One `stdout` of forty
  // megabytes is one piece, and a sink that took it whole would be back to
  // the single-giant-argument problem this file exists to avoid.
  let pending = "";
  let aborted = false;

  /**
   * Write what has accumulated, in pieces no larger than `CHUNK`.
   *
   * The write is its own statement rather than an argument to
   * `onProgress?.()`, and that is not style: an optional call does not
   * evaluate its arguments when the callee is undefined, so a caller that
   * passed no progress callback got a file that was opened, closed, and
   * never written to. `save.test.ts` covers it.
   */
  const flush = async (all: boolean) => {
    while (pending.length >= CHUNK || (all && pending.length > 0)) {
      const cut = pending.length <= CHUNK ? pending.length : boundary(pending, CHUNK);
      const bytes = await api.fileSaveWrite(id, pending.slice(0, cut));
      pending = pending.slice(cut);
      save.onProgress?.(bytes);
    }
  };

  try {
    for await (const piece of save.chunks()) {
      if (save.cancelled?.()) {
        aborted = true;
        break;
      }
      pending += piece;
      if (pending.length >= CHUNK) await flush(false);
    }
    if (aborted) {
      await api.fileSaveAbort(id);
      return null;
    }
    await flush(true);
    return await api.fileSaveFinish(id);
  } catch (e) {
    // `file_save_write` already unlinks after a failed write, and aborting a
    // sink that is no longer open is a no-op — so this is belt and braces
    // for the case where the failure was somewhere else entirely.
    await api.fileSaveAbort(id).catch(() => {});
    throw e;
  }
}

/**
 * Where to cut a string so the two halves are each valid on their own.
 *
 * A surrogate pair split down the middle is a lone surrogate, which is not
 * valid UTF-8 — it survives the JSON that carries it over the bridge and
 * then lands in the file as a replacement character, in the middle of
 * somebody's output, once every megabyte.
 */
function boundary(text: string, at: number): number {
  const code = text.charCodeAt(at - 1);
  return code >= 0xd800 && code <= 0xdbff ? at - 1 : at;
}
