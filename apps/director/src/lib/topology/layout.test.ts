// The drag, as physics rather than as pointer events: take hold of a
// machine, move it, let go, and the fleet must come back to where it was.
import { describe, expect, it } from "vitest";
import { buildGraph, type MeshNode } from "./graph";
import { createLayout } from "./layout";

function fleet(n: number): MeshNode[] {
  return Array.from({ length: n }, (_, i) => ({
    id: "n" + i,
    name: "m-" + i,
    isSelf: i === 0,
    state: "alive" as const,
    tags: ["dc=home", i % 2 ? "gpu" : "cpu"],
    cpu: 20,
    jobs: 0,
    sub: "",
    kind: "server" as const,
  }));
}

function seats(g: ReturnType<typeof buildGraph>) {
  return Array.from({ length: g.n }, (_, i) => [g.x[i], g.y[i]] as const);
}
function centroid(pts: readonly (readonly [number, number])[], skip = -1) {
  let x = 0;
  let y = 0;
  let k = 0;
  pts.forEach((p, i) => {
    if (i === skip) return;
    x += p[0];
    y += p[1];
    k++;
  });
  return [x / k, y / k];
}
/** Ticks a hand holds the machine for — about a second of dragging. */
const HOLD = 60;
const dist = (a: readonly [number, number], b: readonly [number, number]) => Math.hypot(a[0] - b[0], a[1] - b[1]);

describe("dragging a machine", () => {
  for (const n of [5, 40]) {
    it(`n=${n}: it follows the hand, the rest hold their ground, and it springs back`, () => {
      const g = buildGraph(fleet(n));
      const lay = createLayout(g);
      lay.runToRest(5000);
      lay.writeInto(g);
      const rest = seats(g);
      expect(lay.settled()).toBe(true);

      // Take hold of machine 0 and pull it a few link-lengths out of its seat.
      const far: [number, number] = [rest[0][0] + 80, rest[0][1] + 60];
      lay.grab(0, rest[0][0], rest[0][1]);
      expect(lay.settled()).toBe(false);
      lay.drag(0, far[0], far[1]);
      for (let i = 0; i < HOLD; i++) lay.tick(1);
      lay.writeInto(g);
      const held = seats(g);
      // Pinned where the hand is, to float32.
      expect(dist(held[0], far)).toBeLessThan(0.01);
      // Its neighbours gave way — something moved — but the fleet did not
      // slide off after it: the free machines' centre of mass stayed put.
      const moved = held.some((p, i) => i > 0 && dist(p, rest[i]) > 1);
      expect(moved).toBe(true);
      expect(dist(centroid(held, 0) as [number, number], centroid(rest, 0) as [number, number])).toBeLessThan(1);

      // Let go: the pin comes off and the springs take it home.
      lay.release(0);
      let guard = 0;
      while (lay.tick(4) && guard++ < 5000);
      expect(lay.settled()).toBe(true);
      lay.writeInto(g);
      const back = seats(g);
      // Not to the pixel — a relaxation has many nearby minima — but to the
      // same seat by any reading of the picture: well inside a link length
      // of where it started, and nowhere near where it was dropped.
      const off = back.map((p, i) => dist(p, rest[i]));
      expect(off[0], `machine 0 came back ${off[0].toFixed(1)} from its seat`).toBeLessThan(25);
      expect(dist(back[0], far)).toBeGreaterThan(60);
      expect(Math.max(...off.slice(1)), `worst neighbour drift ${Math.max(...off.slice(1)).toFixed(1)}`).toBeLessThan(25);
    });
  }

  it("never cools while held", () => {
    const g = buildGraph(fleet(6));
    const lay = createLayout(g);
    lay.runToRest(5000);
    lay.grab(1, 0, 0);
    for (let i = 0; i < 1000; i++) lay.tick(1);
    expect(lay.settled()).toBe(false);
    expect(lay.held()).toBe(true);
    lay.release(1);
    let guard = 0;
    while (lay.tick(4) && guard++ < 5000);
    expect(lay.settled()).toBe(true);
  });

  it("a snapshot mid-drag lets go and finishes", () => {
    const g = buildGraph(fleet(6));
    const lay = createLayout(g);
    lay.runToRest(5000);
    lay.grab(1, 0, 0);
    lay.drag(1, 500, 500);
    lay.runToRest(2000);
    expect(lay.settled()).toBe(true);
    expect(lay.held()).toBe(false);
  });
});
