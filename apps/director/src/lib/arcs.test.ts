import { describe, expect, it } from "vitest";
import { TAU, annulusPath, gaugeArcTo, layoutRing, minSpan, pointAt, sectorPath } from "./arcs";

/** Every coordinate pair a path visits, in order — the arc commands'
 *  radii and flags skipped. */
function points(d: string): [number, number][] {
  const out: [number, number][] = [];
  const re = /([MLA])([^MLAZ]*)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(d))) {
    const n = m[2].trim().split(/[\s,]+/).map(Number);
    if (m[1] === "A") out.push([n[5], n[6]]);
    else out.push([n[0], n[1]]);
  }
  return out;
}

/** Distance from `p` to the ray from the centre at angle `a`. */
function offRadius(cx: number, cy: number, a: number, p: [number, number]): number {
  const [ux, uy] = [Math.sin(a), -Math.cos(a)];
  const [dx, dy] = [p[0] - cx, p[1] - cy];
  return Math.abs(dx * uy - dy * ux);
}

describe("pointAt", () => {
  it("measures clockwise from twelve o'clock", () => {
    const [x, y] = pointAt(50, 50, 10, 0);
    expect(x).toBeCloseTo(50);
    expect(y).toBeCloseTo(40);
    const [x3, y3] = pointAt(50, 50, 10, Math.PI / 2);
    expect(x3).toBeCloseTo(60);
    expect(y3).toBeCloseTo(50);
  });
});

describe("annulusPath", () => {
  it("is two closed circles, the hole wound the other way", () => {
    const d = annulusPath(50, 50, 50, 40);
    expect(d.match(/Z/g)?.length).toBe(2);
    expect(d).toContain("A50 50 0 1 1");
    expect(d).toContain("A40 40 0 1 0");
  });
});

describe("sectorPath", () => {
  it("cuts its ends parallel to the radius, so a gap has one width from hole to rim", () => {
    const half = 1.5;
    const d = sectorPath(56, 56, 56, 44, 0.4, 2.1, half, 0);
    const [outerStart, outerEnd, innerEnd, innerStart] = points(d);
    // Both corners of the start edge sit exactly `half` off the radius
    // at 0.4, and both of the end edge `half` off the radius at 2.1.
    expect(offRadius(56, 56, 0.4, outerStart)).toBeCloseTo(half, 2);
    expect(offRadius(56, 56, 0.4, innerStart)).toBeCloseTo(half, 2);
    expect(offRadius(56, 56, 2.1, outerEnd)).toBeCloseTo(half, 2);
    expect(offRadius(56, 56, 2.1, innerEnd)).toBeCloseTo(half, 2);
  });

  it("puts every corner point on its circle", () => {
    const d = sectorPath(56, 56, 56, 44, 1, 3, 1.2, 2);
    const onCircle = (p: [number, number]) => Math.hypot(p[0] - 56, p[1] - 56);
    const pts = points(d);
    // M edge, fillet → outer, outer arc → outer, fillet → edge, L edge,
    // fillet → inner, inner arc → inner, fillet → edge.
    expect(onCircle(pts[1])).toBeCloseTo(56, 2);
    expect(onCircle(pts[2])).toBeCloseTo(56, 2);
    expect(onCircle(pts[5])).toBeCloseTo(44, 2);
    expect(onCircle(pts[6])).toBeCloseTo(44, 2);
  });

  it("starts a capped arc exactly on twelve o'clock, never before it", () => {
    // Thickness 12: the cap is a semicircle of radius 6 whose leftmost
    // point is the start of the reading.
    const d = sectorPath(56, 56, 56, 44, 0, Math.PI, 0, 6);
    const [start] = points(d);
    expect(start[0]).toBeCloseTo(56, 3);
    for (const [x, y] of points(d)) if (y < 56) expect(x).toBeGreaterThanOrEqual(56 - 1e-3);
  });

  it("uses the large arc only for a sector of more than half a turn", () => {
    expect(sectorPath(50, 50, 50, 40, 0, 1, 0, 0)).toContain("A50 50 0 0 1");
    expect(sectorPath(50, 50, 50, 40, 0, 5, 0, 0)).toContain("A50 50 0 1 1");
  });

  it("draws nothing once the gaps either side have met", () => {
    expect(sectorPath(50, 50, 50, 40, 0, 0.01, 2, 0)).toBe("");
    expect(sectorPath(50, 50, 50, 40, 1, 1, 0, 0)).toBe("");
  });

  it("gives a span too small for its corners smaller corners, not a crossed shape", () => {
    const span = minSpan(40, 1, 0) * 1.2;
    const d = sectorPath(50, 50, 50, 40, 0, span, 1, 5);
    expect(d).not.toBe("");
    expect(d).not.toContain("NaN");
  });
});

describe("layoutRing", () => {
  const base = { size: 112, thickness: 12, gap: 2.4, corner: 2.4 };

  it("closes a share that is the whole thing into a ring — no seam, no notch", () => {
    const l = layoutRing({ ...base, values: [6] });
    expect(l.shapes).toHaveLength(1);
    expect(l.shapes[0].d).toBe(annulusPath(56, 56, 56, 44));
    expect(l.track).toBe(false);
  });

  it("draws a gauge as a track and one capped arc, and a full gauge as a ring", () => {
    const half = layoutRing({ ...base, values: [50], total: 100 });
    expect(half.track).toBe(true);
    expect(half.shapes).toHaveLength(1);
    expect(half.shapes[0].d).toContain("A6 6"); // the caps
    for (const v of [99.6, 100]) {
      const full = layoutRing({ ...base, values: [v], total: 100 });
      expect(full.shapes[0].d).toBe(annulusPath(56, 56, 56, 44));
    }
  });

  it("keeps the smallest reading a dot at twelve, not a sliver", () => {
    const l = layoutRing({ ...base, values: [0.1], total: 100 });
    expect(l.shapes).toHaveLength(1);
    expect(l.shapes[0].d).not.toBe("");
  });

  it("cuts a breakdown into sectors with nothing under them", () => {
    const l = layoutRing({ ...base, values: [4, 1, 1, 0] });
    expect(l.track).toBe(false);
    expect(l.shapes.map((s) => s.index)).toEqual([0, 1, 2]);
  });

  it("shows a share too small to see, taking the room from the largest", () => {
    // 4 ports in use of 12,001.
    const l = layoutRing({ ...base, values: [11997, 4], total: 12001 });
    expect(l.shapes.map((s) => s.index)).toEqual([0, 1]);
    expect(l.shapes.every((s) => s.d.length > 0)).toBe(true);
  });

  it("draws what the shares leave of the total in the track's place", () => {
    const l = layoutRing({ ...base, values: [3, 2], total: 10 });
    expect(l.shapes.map((s) => s.index)).toEqual([0, 1, null]);
  });

  it("is an empty track with nothing to show", () => {
    expect(layoutRing({ ...base, values: [] })).toEqual({ track: true, shapes: [] });
    expect(layoutRing({ ...base, values: [0, 0], total: 5 })).toEqual({ track: true, shapes: [] });
  });

  it("sweeps in: part of the turn drawn, the rest as track", () => {
    const l = layoutRing({ ...base, values: [6], sweep: 0.5 });
    expect(l.shapes.map((s) => s.index)).toEqual([0, null]);
  });
});

describe("gaugeArcTo", () => {
  function record(frac: number) {
    const calls: [string, number[]][] = [];
    gaugeArcTo(
      {
        moveTo: (...a) => calls.push(["moveTo", a]),
        arc: (...a) => calls.push(["arc", a]),
      },
      0,
      0,
      10,
      2,
      frac,
    );
    return calls;
  }

  it("sets each round cap back half the stroke from the reading's ends", () => {
    const [, [, [, , , s, e]]] = record(0.5);
    const inset = Math.asin(1 / 10);
    expect(s).toBeCloseTo(-Math.PI / 2 + inset);
    expect(e).toBeCloseTo(-Math.PI / 2 + Math.PI - inset);
  });

  it("closes a full reading into a circle, and draws nothing for none", () => {
    const [, [, [, , , s, e]]] = record(1);
    expect(e - s).toBeCloseTo(TAU);
    expect(record(0)).toEqual([]);
  });
});
