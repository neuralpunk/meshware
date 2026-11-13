// The Fleet table's filter, status chips and sort (UX-8).
//
// At five machines the table needed none of this; at sixty-two it was a
// page over 4,000px tall with no way to find one machine, and Enterprise
// starts at fifty. The rules live here rather than in `Nodes.svelte` so
// they are tested, and so "needs attention" means exactly what the mesh's
// health capsule means by it (`attention.ts`).

import type { NodeView } from "./bindings/NodeView";
import type { TopView } from "./bindings/TopView";
import { attentionItems } from "./attention";
import { drainOf } from "./drain";

export type FleetChip = "all" | "attention" | "quiet" | "quarantined" | "held";

export const FLEET_CHIPS: readonly { id: FleetChip; label: string; title: string }[] = [
  { id: "all", label: "All", title: "Every machine in the mesh" },
  {
    id: "attention",
    label: "Needs attention",
    title: "Machines not replying, quarantined, or stopped catching up — the machines the health capsule counts",
  },
  { id: "quiet", label: "Not replying", title: "Suspect or dead: heartbeats have lapsed" },
  { id: "quarantined", label: "Quarantined", title: "Isolated pending investigation" },
  {
    id: "held",
    label: "Not taking work",
    title: "No-execute, held back by the owner, or draining",
  },
];

/** Does this machine take new work? */
export function takesWork(n: NodeView): boolean {
  return n.executing && n.contributes && n.quarantined === null && drainOf(n.tags) === null;
}

export function chipMatches(chip: FleetChip, n: NodeView): boolean {
  switch (chip) {
    case "all":
      return true;
    case "attention":
      return attentionItems({ nodes: [n] }).length > 0;
    case "quiet":
      return n.liveness === "suspect" || n.liveness === "dead";
    case "quarantined":
      return n.quarantined !== null;
    case "held":
      return !takesWork(n);
  }
}

/**
 * Does `n` answer the search box? Case-insensitive substring over what a
 * person would type to find a machine: its name, its id (short or full),
 * a tag, an address, or its build.
 */
export function searchMatches(query: string, n: NodeView): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return q
    .split(/\s+/)
    .every((word) =>
      [n.name, n.shortId, n.nodeId, n.version ?? "", ...n.tags, ...n.addrs].some((f) =>
        f.toLowerCase().includes(word),
      ),
    );
}

export type FleetSortKey = "default" | "name" | "slots" | "work" | "load" | "sync" | "joined";
export type FleetSort = { key: FleetSortKey; desc: boolean };

/** Where a click on a header starts: numbers that mean "more" read best
 *  biggest first, names and dates in their natural order. */
export const SORT_STARTS_DESC: Record<FleetSortKey, boolean> = {
  default: false,
  name: false,
  slots: true,
  work: true,
  load: true,
  sync: true,
  joined: false,
};

/** The next sort after a click on `key`'s header: first click its natural
 *  direction, second the reverse, third back to the default order. */
export function nextSort(cur: FleetSort, key: FleetSortKey): FleetSort {
  if (cur.key !== key) return { key, desc: SORT_STARTS_DESC[key] };
  if (cur.desc === SORT_STARTS_DESC[key]) return { key, desc: !cur.desc };
  return { key: "default", desc: false };
}

export function sortFleet(
  rows: readonly NodeView[],
  sort: FleetSort,
  topOf: (n: NodeView) => TopView | null,
): NodeView[] {
  const byName = (a: NodeView, b: NodeView) => a.name.localeCompare(b.name);
  // A missing reading sorts last whichever way the column runs: "unknown"
  // is not zero, and a column sorted to find the busiest machine should not
  // lead with the ones that have not said.
  const val = (n: NodeView): number | null => {
    const t = topOf(n);
    switch (sort.key) {
      case "slots":
        return t && !t.noExecute ? t.slots : null;
      case "work":
        return t ? t.running + t.queued : null;
      case "load":
        return t?.busy ?? null;
      case "sync":
        return n.sync ? n.sync.behind : null;
      case "joined":
        return n.joinedAtMicros;
      default:
        return null;
    }
  };
  const out = [...rows];
  if (sort.key === "default") {
    // This peer first, then by name: the order the table has always had.
    return out.sort((a, b) => (a.isSelf === b.isSelf ? byName(a, b) : a.isSelf ? -1 : 1));
  }
  if (sort.key === "name") {
    out.sort(byName);
    return sort.desc ? out.reverse() : out;
  }
  return out.sort((a, b) => {
    const va = val(a);
    const vb = val(b);
    if (va === null && vb === null) return byName(a, b);
    if (va === null) return 1;
    if (vb === null) return -1;
    return (sort.desc ? vb - va : va - vb) || byName(a, b);
  });
}

/**
 * A selector naming exactly these machines, for Launch.
 *
 * `|` is the engine's disjunction (`membership.rs`), so this is every one
 * of them and nobody else — a name that is also a tag cannot widen it,
 * because each atom is `name=`.
 */
export function selectorFor(names: readonly string[]): string {
  return names.map((n) => `name=${n}`).join("|");
}
