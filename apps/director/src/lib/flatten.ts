// A record as rows of `key · value` (UX-13).
//
// Topology's Definition tab dumped a node's record as indented JSON: every
// field was there, and none of it was readable at a glance. This turns the
// same record into dotted-path rows — `capacity.cores · 32` — which the tab
// lays out like every other definition in the app, with the JSON one Copy
// away for anybody who wants the record itself.

export type FlatRow = { key: string; value: string };

/** Every leaf of `value`, depth first, keys joined with dots. Arrays of
 *  scalars are one row ("a, b, c"); arrays of objects are indexed. `null`
 *  and empty values say so rather than vanishing. */
export function flattenRecord(value: unknown, prefix = ""): FlatRow[] {
  const out: FlatRow[] = [];
  const leaf = (v: unknown): string => {
    if (v === null || v === undefined) return "—";
    if (typeof v === "string") return v === "" ? "(empty)" : v;
    return String(v);
  };
  const walk = (v: unknown, key: string) => {
    if (Array.isArray(v)) {
      if (v.length === 0) out.push({ key, value: "none" });
      else if (v.every((x) => x === null || typeof x !== "object")) out.push({ key, value: v.map(leaf).join(", ") });
      else v.forEach((x, i) => walk(x, `${key}[${i}]`));
      return;
    }
    if (v !== null && typeof v === "object") {
      const entries = Object.entries(v as Record<string, unknown>);
      if (entries.length === 0) out.push({ key, value: "none" });
      for (const [k, x] of entries) walk(x, key ? `${key}.${k}` : k);
      return;
    }
    out.push({ key, value: leaf(v) });
  };
  walk(value, prefix);
  return out;
}
