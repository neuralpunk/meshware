// A node's drain, read off its tags (Phase C, E9 / U9).
//
// Mirrors `mesh_proto::drain_of` rule for rule, because the reconciler moves
// instances on what that function says and a panel that read the same tags
// differently would describe a drain the mesh is not doing. The Rust side is
// the authority; `drain.test.ts` holds the cases they must agree on.

/** `drain.by=<unix seconds>` or `drain.by=never`. */
export const DRAIN_BY = "drain.by=";
/** Leave `count = "system"` instances where they are. */
export const DRAIN_KEEP_SYSTEM = "drain.keep-system";
/** The node is shutting down (`meshnode drain`), not merely being emptied. */
export const DRAINING = "draining";

export type NodeDrain = {
  /** Unix seconds; `null` is no deadline. A node announcing `draining` with
   *  no `drain.by` — every build before Phase C — reads as 0: move now. */
  deadlineSecs: number | null;
  keepSystem: boolean;
  leaving: boolean;
};

export function drainOf(tags: readonly string[]): NodeDrain | null {
  const leaving = tags.includes(DRAINING);
  const by = tags.find((t) => t.startsWith(DRAIN_BY))?.slice(DRAIN_BY.length);
  if (!leaving && by === undefined) return null;
  let deadlineSecs: number | null;
  if (by === undefined) deadlineSecs = 0;
  else if (by === "never") deadlineSecs = null;
  else {
    // A number nobody could read is not a reason to wait for ever — the
    // same fallback the Rust parse makes.
    const n = /^\d+$/.test(by) ? Number(by) : 0;
    deadlineSecs = Number.isFinite(n) ? n : 0;
  }
  return { deadlineSecs, keepSystem: tags.includes(DRAIN_KEEP_SYSTEM), leaving };
}

/** Is this one of the tags a drain writes? */
export function isDrainTag(tag: string): boolean {
  return tag.startsWith(DRAIN_BY) || tag === DRAIN_KEEP_SYSTEM;
}

function clock(secs: number, nowSecs: number): string {
  const d = new Date(secs * 1000);
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const today = new Date(nowSecs * 1000).toDateString();
  return d.toDateString() === today ? hm : `${d.toDateString().slice(0, 3)} ${hm}`;
}

/** What the drain is doing, in one clause. */
export function drainWords(d: NodeDrain, nowSecs: number): string {
  if (d.leaving) {
    return d.deadlineSecs !== null && d.deadlineSecs <= nowSecs
      ? "shutting down, and moving everything now"
      : "shutting down — its instances are moving first";
  }
  if (d.deadlineSecs === null) {
    return "draining — each instance moves once its replacement proves itself, with no deadline";
  }
  if (d.deadlineSecs <= nowSecs) {
    return "draining — the deadline has passed, and whatever had not moved was stopped";
  }
  return `draining — whatever has not moved by ${clock(d.deadlineSecs, nowSecs)} is stopped`;
}
