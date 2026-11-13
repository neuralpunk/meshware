// Streamed output, kept bounded (H-10, DIR-9).
//
// Activity learned this first: a job's output held as one string that every
// chunk re-created costs O(log size) copying per chunk, keeps the whole
// output resident, and hands the entire `<pre>` back to the DOM each time —
// a job printing megabytes visibly stuttered the window. Launch still did
// exactly that, once for a single run and once per machine for a fan-out.
// Both screens keep their panes in one of these now.

/** The most lines a pane keeps: what a person can actually read. The rest
 *  is what Activity's full log and the history are for. */
export const OUTPUT_MAX_LINES = 2000;

/** One line's cap. A job that prints a 40 MB line of minified JSON has
 *  printed one line, and it must not cost 40 MB to show the start of it. */
export const OUTPUT_MAX_LINE_CHARS = 16_384;

/** All the lines together, so two thousand long ones are still bounded. */
export const OUTPUT_MAX_CHARS = 1_000_000;

/**
 * The last lines of a stream, with the line still being written.
 *
 * Plain rather than reactive: a screen keeps its rings beside its state and
 * copies `text` into the state when it is ready to draw, which is what lets
 * Launch draw a burst of chunks once per frame instead of once per chunk.
 */
export class LineRing {
  #lines: string[] = [];
  #chars = 0;
  /** The tail of a chunk that did not end on a newline. Shown, and held
   *  until the newline arrives — otherwise a line split across two frames
   *  renders as two lines. */
  #partial = "";
  /** Lines dropped off the front, for the pane to say so. */
  dropped = 0;

  constructor(
    readonly maxLines = OUTPUT_MAX_LINES,
    readonly maxLineChars = OUTPUT_MAX_LINE_CHARS,
    readonly maxChars = OUTPUT_MAX_CHARS,
  ) {}

  push(chunk: string) {
    const parts = (this.#partial + chunk).split("\n");
    this.#partial = parts.pop() ?? "";
    for (const line of parts) this.#keep(line);
    // A line that never ends is still a line to be bounded: past the cap,
    // it is kept as a line of its own and the stream carries on after it.
    if (this.#partial.length > this.maxLineChars) {
      this.#keep(this.#partial);
      this.#partial = "";
    }
    this.#trim();
  }

  #keep(line: string) {
    const kept =
      line.length > this.maxLineChars
        ? `${line.slice(0, this.maxLineChars)} … (${(line.length - this.maxLineChars).toLocaleString()} more characters)`
        : line;
    this.#lines.push(kept);
    this.#chars += kept.length;
  }

  #trim() {
    let over = this.#lines.length - this.maxLines;
    let drop = 0;
    while (drop < this.#lines.length && (over > 0 || this.#chars > this.maxChars)) {
      this.#chars -= this.#lines[drop].length;
      drop += 1;
      over -= 1;
    }
    if (drop > 0) {
      this.#lines.splice(0, drop);
      this.dropped += drop;
    }
  }

  /** Everything kept, the unfinished line included. */
  get text(): string {
    const whole = this.#lines.join("\n");
    if (!this.#partial) return whole;
    return whole ? `${whole}\n${this.#partial}` : this.#partial;
  }

  /** The last `n` lines, for a pane too small to show more. */
  tail(n: number): string {
    const all = this.#partial ? [...this.#lines.slice(-n), this.#partial] : this.#lines.slice(-n);
    return all.slice(-n).join("\n");
  }

  reset() {
    this.#lines = [];
    this.#chars = 0;
    this.#partial = "";
    this.dropped = 0;
  }
}
