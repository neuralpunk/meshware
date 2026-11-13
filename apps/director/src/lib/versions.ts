// What the fleet is running, as arithmetic.
//
// This lived in the module script of `components/Software.svelte` while
// Updates was a tab of the node table and only that table needed the
// count. Updates is a panel of Mesh with its own row in the rail now, and
// the rail badges that row with how many nodes are behind — so the count
// has two readers, and a plain module is what keeps them agreeing without
// the sidebar importing the whole rollout screen.

import type { NodeView } from "./api";

/**
 * Compare versions the way `mesh_proto::version_cmp` does: numerically
 * (0.10.0 beats 0.9.0), a pre-release below the release it precedes
 * (0.7.0-rc.1 < 0.7.0), and anything unparseable below everything else.
 */
export function versionCmp(a: string, b: string): number {
  const parse = (v: string) => {
    const m = /^v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+.*)?$/.exec(v.trim());
    if (!m) return null;
    return { core: [Number(m[1]), Number(m[2] ?? 0), Number(m[3] ?? 0)], pre: m[4] ?? null };
  };
  const pa = parse(a);
  const pb = parse(b);
  if (!pa && !pb) return a.trim().localeCompare(b.trim());
  if (!pa) return -1;
  if (!pb) return 1;
  for (let i = 0; i < 3; i++) {
    const d = pa.core[i] - pb.core[i];
    if (d !== 0) return d;
  }
  if (pa.pre === pb.pre) return 0;
  if (pa.pre === null) return 1;
  if (pb.pre === null) return -1;
  const xs = pa.pre.split(".");
  const ys = pb.pre.split(".");
  for (let i = 0; i < Math.max(xs.length, ys.length); i++) {
    if (xs[i] === undefined) return -1;
    if (ys[i] === undefined) return 1;
    const nx = /^\d+$/.test(xs[i]) ? Number(xs[i]) : null;
    const ny = /^\d+$/.test(ys[i]) ? Number(ys[i]) : null;
    if (nx !== null && ny !== null) {
      if (nx !== ny) return nx - ny;
    } else if (nx !== null) return -1;
    else if (ny !== null) return 1;
    else if (xs[i] !== ys[i]) return xs[i].localeCompare(ys[i]);
  }
  return 0;
}

/** What the fleet is running, grouped, newest first. */
export function versionGroupsOf(nodes: NodeView[]): { version: string; nodes: NodeView[] }[] {
  const by = new Map<string, NodeView[]>();
  for (const n of nodes) {
    const v = n.version ?? "unknown";
    by.set(v, [...(by.get(v) ?? []), n]);
  }
  return [...by.entries()]
    .map(([version, group]) => ({ version, nodes: group }))
    .sort((a, b) => {
      if (a.version === "unknown") return 1;
      if (b.version === "unknown") return -1;
      return versionCmp(b.version, a.version);
    });
}

/** The newest version anything in the mesh is running. */
export function newestOf(groups: { version: string }[]): string | null {
  return groups.find((g) => g.version !== "unknown")?.version ?? null;
}

/** Live members — the only ones a rollout can reach or a count should claim. */
export function liveOf(nodes: NodeView[]): NodeView[] {
  return nodes.filter((n) => !n.left && !n.revoked);
}

/** Members a rollout can act on: live, and not this machine's own node.
 *  Director keeps that one at the meshnode it carries, updating it itself
 *  each time it starts (`localnode.rs`) — so no rollout or SSH session is
 *  ever pointed at the machine Director is running on. */
export function fleetOf(nodes: NodeView[]): NodeView[] {
  return liveOf(nodes).filter((n) => !n.isSelf);
}

/** The newer of two versions, either of which may be missing. */
export function newerOf(a: string | null | undefined, b: string | null | undefined): string | null {
  const known = [a, b].filter((v): v is string => !!v && v !== "unknown");
  return known.sort(versionCmp).at(-1) ?? null;
}

/** What the fleet should be on: the newest build anything in the mesh runs,
 *  or the meshnode this Director carries, whichever is newer. The second is
 *  what a rollout from here installs — and what this machine's node is kept
 *  at — so a fleet that has not caught up with it is behind even while every
 *  node agrees with every other. */
export function targetOf(nodes: NodeView[], carried?: string | null): string | null {
  return newerOf(newestOf(versionGroupsOf(liveOf(nodes))), carried);
}

/** How many fleet nodes are not on the target build (`targetOf`). */
export function outdatedCount(nodes: NodeView[], carried?: string | null): number {
  const target = targetOf(nodes, carried);
  return target ? fleetOf(nodes).filter((n) => n.version !== target).length : 0;
}
