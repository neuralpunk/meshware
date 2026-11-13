// The mesh, as a graph — the model behind the topology canvas.
//
// The old picture put this machine at the centre of a 640×360 viewBox and
// hung every peer off it on a fixed ring. That is a drawing of a control
// plane with clients attached, and meshware is not one: every node gossips
// with every other, nobody is a hub, and losing the machine you happen to be
// sitting at costs the mesh nothing. So the picture had to stop implying it.
//
// Two honesty rules shape everything below, and they are worth stating
// plainly because a topology diagram that invents structure is worse than no
// diagram at all:
//
//  1. **We know who *can* talk to whom, not who did.** SWIM membership is
//     full: every node knows every other, and each round it gossips with a
//     random subset that is never published. So for a fleet small enough to
//     draw whole, the complete graph is not an invention — it is the mesh,
//     and a three-machine mesh really is a triangle. Past `FULL_MESH_AT` the
//     complete graph stops being a picture (a hundred machines is 4,950
//     lines), so the lines fall back to the relationship the fleet states
//     about itself: a shared tag.
//  2. **A drawn edge set is a sample, not the truth.** Four hundred machines
//     tagged `dc=east` is eighty thousand pairs, and eighty thousand pairs is
//     a black rectangle. Past a small group size the tag is drawn as a chain
//     through its members rather than a clique — enough to read as a
//     neighbourhood, cheap enough to draw. Hovering a machine still
//     highlights *every* peer that shares a tag with it, drawn edge or not,
//     so the reduction never costs you an answer.
//
// Nothing in this file touches the DOM. It is the part worth testing.

export type Liveness = "alive" | "noexec" | "suspect" | "dead";

/** One machine, flattened to what the canvas needs. Deliberately not
 *  `NodeView`: the renderer should not know what an op log is. */
export type MeshNode = {
  id: string;
  name: string;
  isSelf: boolean;
  state: Liveness;
  tags: string[];
  /** 0–100, or null when the machine has not reported. */
  cpu: number | null;
  jobs: number;
  /** The caption under the name — "3 jobs", "no reply", "no-execute". */
  sub: string;
  /** What kind of box it is, for the glyph on its chip: one with
   *  accelerators, one that runs work, or a peer that only watches. */
  kind: "gpu" | "server" | "observer";
};

export type MeshGraph = {
  n: number;
  nodes: MeshNode[];
  /** Settled world coordinates, centred on the origin. */
  x: Float32Array;
  y: Float32Array;
  /** Which tag-group each machine belongs to, and what that group is called. */
  group: Uint16Array;
  groupName: string[];
  /** Undirected edge list, two entries per edge. */
  edges: Int32Array;
  /** Per edge: 1 = peers because everyone is, 2 = something in common. */
  strength: Uint8Array;
  /** Per edge: how long this link *wants* to be, as a multiple of the base
   *  link distance. Per node: how hard it shoves its neighbours, and how
   *  firmly it is drawn toward the middle. Fed to d3-force. See `TEMPER`. */
  restLen: Float32Array;
  push: Float32Array;
  gravity: Float32Array;
  m: number;
  /** True when the whole mesh is drawn, rather than a tag sample of it. */
  complete: boolean;
  /** Median distance to a nearest neighbour, in world units. The renderer
   *  sizes machines off this so a three-node mesh gets big chips and a
   *  five-thousand-node one gets dots, without a magic number per size. */
  spacing: number;
  /** CSR adjacency over `edges` — what a hover walks. */
  off: Int32Array;
  adj: Int32Array;
  deg: Uint16Array;
  maxDeg: number;
  /** Machines sorted by group then name: the matrix's row order, and the
   *  only order in which a block diagonal means anything. */
  order: Int32Array;
  /** Where each group starts in `order`, for separators and labels. */
  gstart: Int32Array;
  /** The settled layout's box: half-extents and where its middle actually
   *  is. Fitting to the origin instead of to the middle is what left a small
   *  mesh huddled against one edge with a third of the pane empty. */
  bounds: { rx: number; ry: number; cx: number; cy: number };
  /** Every machine's full shared-tag neighbourhood, on demand. */
  sharesTagWith(i: number): Int32Array;
};

/** The world the layout lives in. Screen scale is the viewport's business;
 *  in here everything is world units and the frame is ~16:9, because a round
 *  blob in a widescreen pane wastes a third of the pane. */
export const WORLD_RY = 500;
export const WORLD_RX = WORLD_RY * 1.6;

/** At or below this many machines the mesh is drawn complete, because it is
 *  complete — everyone is everyone's peer. Sixteen is 120 lines, which reads
 *  as a texture; thirty-two would be 496, which reads as a blot.
 *
 *  This is also what stops a small homogeneous mesh coming out empty. Three
 *  machines that all carry one tag share no *distinguishing* tag, and drawing
 *  nothing was the honest-looking wrong answer: they are a triangle. */
const FULL_MESH_AT = 16;

/** Below this many members a tag is drawn as a clique — every pair, which is
 *  the truth. Above it the tag becomes a chain through its members. Eight is
 *  where a clique stops reading as a neighbourhood and starts reading as a
 *  blot: 8 machines is 28 lines. */
const CLIQUE_MAX = 8;
/** Forward links per member once a tag is too big for a clique. Two, not
 *  one: a single chain is a necklace, and two is a band you can see the
 *  thickness of. */
const CHAIN_K = 2;

/**
 * How far the solve is allowed to be uneven — the difference between a
 * diagram and a photograph of one.
 *
 * A force layout over a symmetric graph has a symmetric answer. Five
 * machines that are all each other's peers, with one spring constant and one
 * repulsion, settle into a regular pentagon every time — geometrically
 * correct, and it reads as a shape somebody drew rather than a fleet that
 * came to rest. Real meshes are lumpy.
 *
 * So the constants are not constants. Every link gets its own rest length,
 * every machine its own shove, its own pull toward its group and its own
 * pull toward the middle — all derived from the node ids, which means the
 * layout is still a genuine relaxation, still converges, and still lands in
 * the same place every time you open the page. It is just no longer a shape
 * with a name.
 *
 * `gravity` is the one that matters most and is least obvious. Uneven
 * springs slide machines *around* a ring; they stay on it, because a radial
 * pull that is the same for everybody puts everybody the same distance out.
 * Letting some machines sit closer to the middle than others is what turns
 * the ring into a drift.
 *
 * These feed d3-force's accessors — link distance, charge strength, and the
 * x/y centring pulls — so the unevenness is inside the simulation rather
 * than applied to its result.
 */
const TEMPER = { rest: 0.4, push: 0.3, gravity: 0.4 };

/** FNV-1a. The jitter that breaks a symmetric seeding has to be stable for
 *  the life of a machine — derived from the node id, never from its index,
 *  so a fleet that gains a member does not reshuffle every seat in it. */
function hash32(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

/**
 * Build the graph: group by tag, derive edges, settle a layout.
 *
 * Called only when the *membership or tagging* changes, never on a poll that
 * returned the same fleet — a full solve is a fleet-sized cost and the seats
 * it produces are identical.
 */
/** How hard a machine is working, as five bands.
 *
 * Banded rather than a continuous ramp because nobody reads 61% off a hue —
 * "steady" and "hot" are the words an operator already uses, and a band is
 * the only thing a legend can honestly show. Lives here so the painter and
 * the legend cannot drift apart; the colours are `--load-*` in app.css.
 */
export const LOAD_BANDS = [
  { upTo: 15, label: "idle", tone: "load-idle" },
  { upTo: 40, label: "light", tone: "load-light" },
  { upTo: 65, label: "steady", tone: "load-steady" },
  { upTo: 85, label: "busy", tone: "load-busy" },
  { upTo: Infinity, label: "hot", tone: "load-hot" },
] as const;

/** Which band a reading falls in, or -1 for a machine that has not reported
 *  one at all. */
export function loadBand(cpu: number | null): number {
  if (cpu == null) return -1;
  for (let i = 0; i < LOAD_BANDS.length; i++) if (cpu < LOAD_BANDS[i].upTo) return i;
  return LOAD_BANDS.length - 1;
}

export function buildGraph(nodes: MeshNode[]): MeshGraph {
  const n = nodes.length;
  const x = new Float32Array(n);
  const y = new Float32Array(n);
  const group = new Uint16Array(n);

  // -- grouping ------------------------------------------------------------
  //
  // A machine's group is the most specific meaningful tag it carries. A tag
  // every peer holds says nothing about grouping — it would pull the whole
  // fleet into one ball — and a tag exactly one peer holds is not a group
  // either. What is left, smallest first, is the tag that actually places a
  // machine: `rack-c` beats `dc=east` beats `role=worker`.
  const held = new Map<string, number>();
  for (const node of nodes) for (const tag of new Set(node.tags)) held.set(tag, (held.get(tag) ?? 0) + 1);
  /** Two different questions, and conflating them is what emptied the
   *  three-machine mesh. For *placing* a machine, a tag every peer carries
   *  says nothing — it would pull the whole fleet into one ball. For
   *  *relating* two machines it says plenty: it is the thing they have in
   *  common, and in a fleet where it is the only tag anybody holds, it is the
   *  only thing there is to draw. */
  const groupTag = (tag: string) => {
    const c = held.get(tag) ?? 0;
    return c >= 2 && c < n;
  };
  const linkTag = (tag: string) => (held.get(tag) ?? 0) >= 2;
  const meaningful = groupTag;
  const UNGROUPED = "untagged";
  const groupOf = nodes.map((node) => {
    let best = "";
    let bestCount = Infinity;
    for (const tag of new Set(node.tags)) {
      if (!meaningful(tag)) continue;
      const c = held.get(tag)!;
      // Smallest wins; the tag's own name breaks a tie, so the same fleet
      // groups the same way on every machine that draws it.
      if (c < bestCount || (c === bestCount && tag < best)) {
        best = tag;
        bestCount = c;
      }
    }
    return best || UNGROUPED;
  });
  const groupName = [...new Set(groupOf)].sort();
  const groupIndex = new Map(groupName.map((g, i) => [g, i]));
  for (let i = 0; i < n; i++) group[i] = groupIndex.get(groupOf[i])!;
  const gCount = groupName.length;

  // -- edges ---------------------------------------------------------------
  const byTag = new Map<string, number[]>();
  for (let i = 0; i < n; i++) {
    for (const tag of new Set(nodes[i].tags)) {
      if (!linkTag(tag)) continue;
      const list = byTag.get(tag);
      if (list) list.push(i);
      else byTag.set(tag, [i]);
    }
  }
  const seen = new Map<number, number>();
  const ea: number[] = [];
  const eb: number[] = [];
  const es: number[] = [];
  /** `weight` 1 is "these two are peers because everyone is"; 2 is "these two
   *  have something in common". An edge can be upgraded, never demoted. */
  const link = (a: number, b: number, weight: number) => {
    if (a === b) return;
    const key = a < b ? a * n + b : b * n + a;
    const at = seen.get(key);
    if (at !== undefined) {
      if (weight > es[at]) es[at] = weight;
      return;
    }
    seen.set(key, ea.length);
    ea.push(a);
    eb.push(b);
    es.push(weight);
  };

  // A mesh small enough to draw whole is drawn whole.
  if (n <= FULL_MESH_AT) for (let a = 0; a < n; a++) for (let b = a + 1; b < n; b++) link(a, b, 1);
  // Tags in name order, members in id order: a deterministic edge set, so
  // two people looking at the same fleet are looking at the same picture.
  for (const tag of [...byTag.keys()].sort()) {
    const list = byTag.get(tag)!;
    list.sort((a, b) => (nodes[a].id < nodes[b].id ? -1 : 1));
    if (list.length <= CLIQUE_MAX) {
      for (let a = 0; a < list.length; a++) for (let b = a + 1; b < list.length; b++) link(list[a], list[b], 2);
    } else {
      for (let a = 0; a < list.length; a++)
        for (let k = 1; k <= CHAIN_K; k++) link(list[a], list[(a + k) % list.length], 2);
    }
  }

  // Nobody floats.
  //
  // A machine whose tags are unique to it — the Director with `role=control`,
  // a laptop with `no-execute` — comes out of the loop above with no lines at
  // all, and a machine drawn alone in the corner reads as a machine that has
  // fallen out of the mesh. It has not: membership is full, and it gossips
  // with every other machine in there. So it is drawn as a peer of a few of
  // them, spread across the fleet rather than all pointing at one place, and
  // at the lighter of the two weights because that is the weaker claim.
  if (n > 1) {
    const touched = new Int32Array(n);
    for (let e = 0; e < ea.length; e++) {
      touched[ea[e]]++;
      touched[eb[e]]++;
    }
    const stride = Math.max(1, Math.floor(n / 4));
    for (let i = 0; i < n; i++) {
      if (touched[i] > 0) continue;
      for (let k = 1; k <= Math.min(3, n - 1); k++) link(i, (i + k * stride) % n, 1);
    }
  }

  const m = ea.length;
  const edges = new Int32Array(m * 2);
  const strength = Uint8Array.from(es);

  // -- the unevenness ------------------------------------------------------
  const nodeHash = new Uint32Array(n);
  for (let i = 0; i < n; i++) nodeHash[i] = hash32(nodes[i].id);
  const unit = (h: number) => (h >>> 8) / 16777216; // 0…1
  const push = new Float32Array(n);
  const gravity = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const h = nodeHash[i];
    push[i] = 1 + (unit(h) - 0.5) * 2 * TEMPER.push;
    gravity[i] = 1 + (unit(Math.imul(h ^ 0x27d4eb2f, 0xc2b2ae35)) - 0.5) * 2 * TEMPER.gravity;
  }
  const restLen = new Float32Array(m);
  for (let e = 0; e < m; e++) {
    const mixed = (nodeHash[ea[e]] ^ Math.imul(nodeHash[eb[e]], 0x85ebca6b)) >>> 0;
    restLen[e] = 1 + (unit(mixed) - 0.5) * 2 * TEMPER.rest;
  }
  for (let e = 0; e < m; e++) {
    edges[e * 2] = ea[e];
    edges[e * 2 + 1] = eb[e];
  }

  const deg = new Uint16Array(n);
  for (let e = 0; e < m; e++) {
    deg[ea[e]]++;
    deg[eb[e]]++;
  }
  const off = new Int32Array(n + 1);
  for (let i = 0; i < n; i++) off[i + 1] = off[i] + deg[i];
  const cursor = off.slice(0, n);
  const adj = new Int32Array(m * 2);
  for (let e = 0; e < m; e++) {
    adj[cursor[ea[e]]++] = eb[e];
    adj[cursor[eb[e]]++] = ea[e];
  }
  let maxDeg = 1;
  for (let i = 0; i < n; i++) if (deg[i] > maxDeg) maxDeg = deg[i];

  // Positions are left to `layout.ts`: d3-force seeds anything unplaced on a
  // phyllotaxis spiral of its own, and the group centroids this used to
  // scatter machines around were a worse starting guess than that — they
  // pre-decided the clustering the simulation is supposed to discover.
  //
  // Bounds and spacing are likewise measured by `remeasure`, once the
  // simulation has actually put the machines somewhere.
  const gSize = new Int32Array(gCount);
  for (let i = 0; i < n; i++) gSize[group[i]]++;

  const g: MeshGraph = {
    n,
    nodes,
    x,
    y,
    group,
    groupName,
    edges,
    strength,
    restLen,
    push,
    gravity,
    m,
    complete: n <= FULL_MESH_AT,
    spacing: 1,
    off,
    adj,
    deg,
    maxDeg,
    order: new Int32Array(n),
    gstart: new Int32Array(gCount + 1),
    bounds: { rx: WORLD_RX, ry: WORLD_RY, cx: 0, cy: 0 },
    sharesTagWith(i: number) {
      // The full truth, computed on the one machine somebody asked about
      // rather than stored for all of them.
      const out = new Set<number>();
      for (const tag of new Set(nodes[i].tags)) {
        if (!linkTag(tag)) continue;
        for (const j of byTag.get(tag) ?? []) if (j !== i) out.add(j);
      }
      return Int32Array.from(out);
    },
  };

  // Machines sorted by group then name: the matrix's row order, and the only
  // order in which a block diagonal means anything.
  const sorted = Array.from({ length: n }, (_, i) => i).sort((a, b) =>
    group[a] !== group[b] ? group[a] - group[b] : nodes[a].name.localeCompare(nodes[b].name),
  );
  g.order = Int32Array.from(sorted);
  let at = 0;
  for (let gi = 0; gi < gCount; gi++) {
    g.gstart[gi] = at;
    at += gSize[gi];
  }
  g.gstart[gCount] = at;
  return g;
}

/** Recompute the box and the spacing after positions move — a re-settle
 *  changes both, and a fit that still frames where the fleet used to be is a
 *  fit that frames nothing. */
export function remeasure(g: MeshGraph): void {
  remeasureInto(g, g.x, g.y, g.n);
}

function remeasureInto(g: MeshGraph, x: Float32Array, y: Float32Array, n: number): void {
  let minX = Infinity;
  let maxX = -Infinity;
  let minY = Infinity;
  let maxY = -Infinity;
  for (let i = 0; i < n; i++) {
    if (x[i] < minX) minX = x[i];
    if (x[i] > maxX) maxX = x[i];
    if (y[i] < minY) minY = y[i];
    if (y[i] > maxY) maxY = y[i];
  }
  g.bounds = {
    rx: Math.max(1, (maxX - minX) / 2) * 1.1,
    ry: Math.max(1, (maxY - minY) / 2) * 1.16,
    cx: (minX + maxX) / 2,
    cy: (minY + maxY) / 2,
  };
  g.spacing = medianSpacing(x, y, n);
}

/**
 * One pass of the relaxation.
 *
 * Repulsion runs over a uniform grid rather than every pair: a machine only
 * pushes the ones inside its own cell and the eight around it, which is what
 * turns an O(n²) solve into an O(n) one and is the whole reason five thousand
 * nodes is a supported number rather than a warning.
 *
 * Exported because the canvas re-runs a handful of passes when a machine
 * joins or leaves, rather than re-solving the fleet from scratch.
 */

// ---------------------------------------------------------------------------
// Picking
//
// A hover has to answer "which machine is under the cursor" in constant time
// at any fleet size. The same uniform grid, kept between frames.
// ---------------------------------------------------------------------------
export type PointIndex = {
  x: Float32Array;
  y: Float32Array;
  x0: number;
  y0: number;
  cell: number;
  nx: number;
  ny: number;
  head: Int32Array;
  next: Int32Array;
};

export function makeIndex(x: Float32Array, y: Float32Array, n: number): PointIndex {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (let i = 0; i < n; i++) {
    if (x[i] < x0) x0 = x[i];
    if (x[i] > x1) x1 = x[i];
    if (y[i] < y0) y0 = y[i];
    if (y[i] > y1) y1 = y[i];
  }
  if (!Number.isFinite(x0)) {
    x0 = y0 = 0;
    x1 = y1 = 1;
  }
  const cell = Math.max(4, Math.hypot(x1 - x0, y1 - y0) / Math.max(6, Math.sqrt(n)));
  const nx = Math.max(1, Math.ceil((x1 - x0 + 1) / cell));
  const ny = Math.max(1, Math.ceil((y1 - y0 + 1) / cell));
  const head = new Int32Array(nx * ny).fill(-1);
  const next = new Int32Array(n);
  for (let i = 0; i < n; i++) {
    const cx = Math.min(nx - 1, Math.max(0, ((x[i] - x0) / cell) | 0));
    const cy = Math.min(ny - 1, Math.max(0, ((y[i] - y0) / cell) | 0));
    const c = cy * nx + cx;
    next[i] = head[c];
    head[c] = i;
  }
  return { x, y, x0, y0, cell, nx, ny, head, next };
}

/** Nearest point within `maxD` world units, or -1. */
export function nearest(ix: PointIndex, wx: number, wy: number, maxD: number): number {
  const cx = Math.min(ix.nx - 1, Math.max(0, ((wx - ix.x0) / ix.cell) | 0));
  const cy = Math.min(ix.ny - 1, Math.max(0, ((wy - ix.y0) / ix.cell) | 0));
  const rad = Math.max(1, Math.ceil(maxD / ix.cell));
  let best = -1;
  let bestD = maxD * maxD;
  for (let gy = Math.max(0, cy - rad); gy <= Math.min(ix.ny - 1, cy + rad); gy++) {
    for (let gx = Math.max(0, cx - rad); gx <= Math.min(ix.nx - 1, cx + rad); gx++) {
      for (let i = ix.head[gy * ix.nx + gx]; i !== -1; i = ix.next[i]) {
        const dx = ix.x[i] - wx;
        const dy = ix.y[i] - wy;
        const d = dx * dx + dy * dy;
        if (d < bestD) {
          bestD = d;
          best = i;
        }
      }
    }
  }
  return best;
}

/**
 * The median distance from a machine to its nearest neighbour.
 *
 * This is the number the renderer sizes marks off, and it is measured rather
 * than assumed for one reason: the alternative — a pixel size derived from
 * the node count — got a three-machine mesh wrong in both directions at
 * once. Three chips the size of a hundred-machine fleet's, adrift in a pane
 * built for hundreds. Density is what should set the size of a thing, and
 * density is a property of the settled layout, not of `n`.
 */
function medianSpacing(x: Float32Array, y: Float32Array, n: number): number {
  if (n < 2) return 200;
  const ix = makeIndex(x, y, n);
  const out = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    const cx = Math.min(ix.nx - 1, Math.max(0, ((x[i] - ix.x0) / ix.cell) | 0));
    const cy = Math.min(ix.ny - 1, Math.max(0, ((y[i] - ix.y0) / ix.cell) | 0));
    let best = Infinity;
    // Two rings of cells, then give up and take the cell size: a lone
    // machine at the rim has no near neighbour and should not drag the
    // median to infinity.
    for (let r = 1; r <= 2 && best === Infinity; r++) {
      for (let gy = Math.max(0, cy - r); gy <= Math.min(ix.ny - 1, cy + r); gy++) {
        for (let gx = Math.max(0, cx - r); gx <= Math.min(ix.nx - 1, cx + r); gx++) {
          for (let j = ix.head[gy * ix.nx + gx]; j !== -1; j = ix.next[j]) {
            if (j === i) continue;
            const d = (x[j] - x[i]) ** 2 + (y[j] - y[i]) ** 2;
            if (d < best) best = d;
          }
        }
      }
    }
    out[i] = best === Infinity ? ix.cell * ix.cell : best;
  }
  const sorted = Array.from(out).sort((a, b) => a - b);
  return Math.sqrt(sorted[sorted.length >> 1]) || 200;
}

/** A stable description of "which fleet is this, tagged how". The layout is
 *  re-solved when this changes and never otherwise — `others` is rebuilt from
 *  a poll every second, and re-solving a fleet that has not changed is a
 *  fleet-sized cost paid for nothing. */
export function fleetKey(nodes: MeshNode[]): string {
  // Name is in here as well as id and tags: the matrix orders rows by name,
  // so a rename re-seats the fleet — and everything downstream assumes that
  // an unchanged key means the same machines in the same order.
  let key = String(nodes.length);
  for (const node of nodes) key += "|" + node.id + "~" + node.name + "~" + node.tags.join(",");
  return key;
}
