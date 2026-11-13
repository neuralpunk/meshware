// The answer set as text: what the JSON pane draws, what Copy puts on the
// clipboard, and what Save streams to disk — one set of functions, so all
// three agree by construction.
//
// Everything here is written to be safe at the size an answer set actually
// reaches. A fan-out of six hundred machines each returning a megabyte of
// stdout is a 600 MB document, and the rules that follow from that are:
//
//   · **Never touch a whole value to learn something about it.** `.trim()`
//     on a 40 MB string allocates 40 MB to answer "is this blank"; a
//     regex that stops at the first non-space answers the same question
//     for free. Same for the lowercase copy a naive filter makes.
//   · **Slice before you transform.** `s.replace(/\s+/g, " ")` on a value
//     bound for a 280 px cell copies the whole value to produce 200
//     characters of it. Slice, then collapse.
//   · **Know the shape's line count without rendering it.** `rowLines`
//     below is O(1) because `JSON.stringify` puts a string — newlines and
//     all — on one line. That is what makes the JSON pane virtualisable:
//     the offset of row 90,000 is arithmetic, not a 600 MB render.
//
// `rowLines` and `rowJson` have to stay in step or the JSON pane's
// scrollbar lies about where things are, so a test asserts they do.
import type { ResultRowView } from "./api";
import { fmtBytes } from "./bytes";

/** Blank, without allocating a copy to find out. */
export function hasContent(v: string): boolean {
  return v.length > 0 && /\S/.test(v);
}

/** The four every job publishes whether or not it declares them. */
const IMPLICIT = ["stdout", "stdout_json", "stderr", "exit_code"];

/**
 * Which output keys any row actually published something under.
 *
 * Declared outputs lead; the four that arrive for free come after, in
 * their own order — what the author named is what they look for first.
 */
export function outputKeys(rows: readonly ResultRowView[]): string[] {
  const seen = new Set<string>();
  for (const r of rows) {
    for (const [k, v] of r.outputs) {
      if (!seen.has(k) && hasContent(v)) seen.add(k);
    }
  }
  return [...seen].sort((a, b) => {
    const ai = IMPLICIT.indexOf(a);
    const bi = IMPLICIT.indexOf(b);
    return (ai < 0 ? -1 : ai) - (bi < 0 ? -1 : bi);
  });
}

/** A row that did not do what was asked: a non-zero exit, or a failed expectation. */
export function isFailure(r: ResultRowView): boolean {
  return r.exitCode !== 0 || r.expectPassed === false;
}

/** One row's value under a key, or `""`. */
export function valueOf(r: ResultRowView, key: string): string {
  return r.outputs.find(([k]) => k === key)?.[1] ?? "";
}

// -- sizes --------------------------------------------------------------------

/**
 * Roughly how large the document will be, from lengths alone.
 *
 * Close enough to draw a progress bar against and to decide whether the
 * clipboard is the wrong tool, and — the point — it never reads a
 * character of any value.
 */
export function estimateBytes(rows: readonly ResultRowView[]): number {
  let n = 0;
  for (const r of rows) {
    n += r.node.length + r.jobId.length + r.state.length + r.expectDetail.length + 96;
    for (const [k, v] of r.outputs) n += k.length + v.length + 12;
  }
  return n;
}

// The shared formatter (UX-19), re-exported where callers already look.
export { fmtBytes };

/** `object:mw1…#1048576` reads as an object reference, not as an id. */
export function objectRef(v: string): string | null {
  const m = v.match(/^object:(\S+)#(\d+)$/);
  return m ? `‹object · ${fmtBytes(Number(m[2]))}›` : null;
}

/**
 * Slice without splitting a surrogate pair, so a cut never lands in the
 * middle of an emoji and leaves a lone half behind.
 */
export function sliceSafe(s: string, n: number): string {
  if (s.length <= n) return s;
  const code = s.charCodeAt(n - 1);
  return s.slice(0, code >= 0xd800 && code <= 0xdbff ? n - 1 : n);
}

/**
 * One line of a value, for a table cell: whitespace collapsed, capped.
 *
 * The slice comes first on purpose — collapsing a 40 MB value to produce
 * 200 characters of it is 40 MB of work per cell per render.
 */
export function cellPreview(v: string, max = 200): string {
  const ref = objectRef(v);
  if (ref) return ref;
  const head = sliceSafe(v, max * 3).replace(/\s+/g, " ").trimStart();
  return head.length > max ? `${sliceSafe(head, max)}…` : head;
}

// -- the document -------------------------------------------------------------

/**
 * How many lines `rowJson` will emit for this row — without emitting them.
 *
 * `JSON.stringify` escapes the newlines inside a string, so a value's
 * *content* never adds a line: the count falls out of the shape alone.
 * Keep in step with `rowJson`; `answers.test.ts` checks that it is.
 */
export function rowLines(r: ResultRowView): number {
  const outputs = r.outputs.length === 0 ? 1 : r.outputs.length + 2;
  return 6 + outputs + (r.expectPassed === null ? 0 : 2);
}

/** 4900 → "4,900". Not `toLocaleString`: this ends up inside a JSON string
    that a test compares and a person may diff, so it must not depend on
    which locale the window happens to be running in. */
function grouped(n: number): string {
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

function shortened(s: string, at: number): string {
  const kept = sliceSafe(s, at);
  return `${kept}…[+${grouped(s.length - kept.length)} characters]`;
}

/**
 * One row, as the array element `meshnode run … --json` prints.
 *
 * Built line by line rather than by `JSON.stringify(row, null, 2)` for one
 * reason: `clip`. Shortening a value for the screen has to happen *before*
 * the value is serialised, or a 40 MB stdout is copied twice on the way to
 * a 900-character line. `null` clips nothing and is what Copy and Save
 * use.
 */
export function rowJson(
  r: ResultRowView,
  indent = "  ",
  clip: number | null = null,
  comma = false,
): string {
  return rowJsonLines_(r, indent, clip, comma).join("\n");
}

/**
 * The same row, as its lines.
 *
 * Save walks these rather than the joined string, so the largest thing it
 * ever holds is one line — which for a 40 MB `stdout` is still 40 MB, but
 * not 40 MB *plus* the row it was joined into plus the document it was
 * concatenated onto. Peak memory during a save is a property worth one
 * indirection.
 */
export function rowJsonParts(
  r: ResultRowView,
  indent = "  ",
  clip: number | null = null,
  comma = false,
): string[] {
  return rowJsonLines_(r, indent, clip, comma);
}

function rowJsonLines_(
  r: ResultRowView,
  indent: string,
  clip: number | null,
  comma: boolean,
): string[] {
  const i2 = `${indent}  `;
  const i3 = `${i2}  `;
  const str = (s: string) =>
    JSON.stringify(clip !== null && s.length > clip ? shortened(s, clip) : s);

  const out: string[] = [`${indent}{`];
  out.push(`${i2}"node": ${str(r.node)},`);
  out.push(`${i2}"job_id": ${str(r.jobId)},`);
  out.push(`${i2}"state": ${str(r.state)},`);
  out.push(`${i2}"exit_code": ${r.exitCode === null ? "null" : r.exitCode},`);
  const tail = r.expectPassed === null ? "" : ",";
  if (r.outputs.length === 0) {
    out.push(`${i2}"outputs": {}${tail}`);
  } else {
    out.push(`${i2}"outputs": {`);
    r.outputs.forEach(([k, v], i) => {
      out.push(`${i3}${JSON.stringify(k)}: ${str(v)}${i === r.outputs.length - 1 ? "" : ","}`);
    });
    out.push(`${i2}}${tail}`);
  }
  if (r.expectPassed !== null) {
    out.push(`${i2}"expect_passed": ${r.expectPassed},`);
    out.push(`${i2}"expect_detail": ${str(r.expectDetail)}`);
  }
  out.push(`${indent}}${comma ? "," : ""}`);
  return out;
}

/**
 * The whole document, a row at a time.
 *
 * A generator rather than a string so that Save never holds more than one
 * row, and so the loop that drains it can hand the event loop a turn
 * between chunks. Copy has to concatenate — the clipboard takes one
 * value — but even there the pieces arrive one row at a time rather than
 * as a second full copy of the set.
 */
export function* documentChunks(
  rows: readonly ResultRowView[],
  clip: number | null = null,
): Generator<string> {
  if (rows.length === 0) {
    yield "[]\n";
    return;
  }
  yield "[\n";
  for (let i = 0; i < rows.length; i += 1) {
    for (const line of rowJsonParts(rows[i], "  ", clip, i < rows.length - 1)) {
      yield line;
      yield "\n";
    }
  }
  yield "]\n";
}

/** The document as one string. Only for the clipboard, which needs one. */
export function documentText(rows: readonly ResultRowView[], clip: number | null = null): string {
  let out = "";
  for (const piece of documentChunks(rows, clip)) out += piece;
  return out;
}

/** A user's filter, as a literal rather than as a pattern. */
export function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Does this row match?
 *
 * Case-insensitivity is the regex's `i` flag rather than
 * `value.toLowerCase().includes(q)`, and that is the whole trick: the
 * regex scans in place and stops at the first hit, where the lowercase
 * copy allocates the value again — per row, per keystroke.
 */
export function rowMatches(r: ResultRowView, re: RegExp): boolean {
  if (re.test(r.node)) return true;
  for (const [k, v] of r.outputs) {
    if (re.test(k) || re.test(v)) return true;
  }
  return re.test(r.expectDetail);
}

/** A filename a person will recognise a week later. */
export function suggestedName(source: string): string {
  const stamp = new Date()
    .toISOString()
    .slice(0, 16)
    .replace(/[-:]/g, "")
    .replace("T", "-");
  const tag = source.trim().replace(/[^A-Za-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");
  return `answers-${tag ? `${tag}-` : ""}${stamp}.json`;
}
