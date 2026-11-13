// The geometry of every ring Director draws (2026-09-22).
//
// The first `Ring` drew its arcs as dashes on a stroked circle
// (`stroke-dasharray`), rotated a quarter turn by CSS. That is an
// approximation of an arc, not an arc, and it showed: two round caps
// butting into each other left a pinched notch at twelve o'clock on
// every full ring; a stray gap angle nudged gauges off twelve; the gaps
// between segments were wedges, wider at the rim than at the hole; and
// the track showed grey through each of them. The rings read as drawn
// by hand.
//
// So every shape here is a closed, filled outline computed exactly: an
// annulus, or an annular sector whose ends are either
//
//   - **cut**: a straight edge parallel to the radius, set back from it
//     by half the gap, so the gap between two neighbours has the same
//     width from the hole to the rim — a knife cut, not a wedge — with
//     its two corners on each circle softened by a true circular fillet
//     tangent to both the edge and the circle; or
//   - **capped**: the fillet grown to half the ring's thickness, which
//     is a semicircle whose outermost point lies exactly on the end
//     angle — so a gauge starts *at* twelve o'clock and never before it.
//
// Angles are radians, clockwise from twelve o'clock, in screen
// coordinates (y down). Nothing here knows about Svelte, SVG elements or
// canvas beyond the path string, so it is tested on its own.

export const TAU = Math.PI * 2;

/** Three decimals is a thousandth of a pixel: exact on any screen, and a
 *  path string that stays readable in the inspector. */
const f = (n: number) => Math.round(n * 1000) / 1000;

type Pt = readonly [number, number];

/** The point at angle `a` (clockwise from twelve) on a circle of radius `r`. */
export function pointAt(cx: number, cy: number, r: number, a: number): Pt {
  return [cx + r * Math.sin(a), cy - r * Math.cos(a)];
}

/** A whole ring: the outer circle clockwise, the hole counter-clockwise,
 *  so it fills correctly under either fill rule. */
export function annulusPath(cx: number, cy: number, ro: number, ri: number): string {
  const circle = (r: number, sweep: 0 | 1) =>
    `M${f(cx)} ${f(cy - r)}A${f(r)} ${f(r)} 0 1 ${sweep} ${f(cx)} ${f(cy + r)}` +
    `A${f(r)} ${f(r)} 0 1 ${sweep} ${f(cx)} ${f(cy - r)}Z`;
  return ri > 0 ? circle(ro, 1) + circle(ri, 0) : circle(ro, 1);
}

/**
 * The widest angle one end of a sector takes up on the inner circle —
 * the binding constraint, since the hole is where a sector is narrowest.
 */
function endAngle(ri: number, half: number, k: number): number {
  const d = ri + k;
  const s = (half + k) / d;
  return s >= 1 ? Infinity : Math.asin(s);
}

/**
 * The smallest span a sector with these ends can have and still be drawn
 * without its fillets crossing: two ends' worth of inner-circle angle.
 */
export function minSpan(ri: number, half: number, k: number): number {
  return 2 * endAngle(ri, half, k);
}

/**
 * An annular sector from `a0` to `a1`, with each end set back by `half`
 * (half the gap to its neighbour, in px) and its corners filleted at
 * radius `corner`. `corner = (ro - ri) / 2` and `half = 0` is a
 * round-capped arc.
 *
 * A span too small for the corners asked for gets smaller corners, down
 * to square ones; a span too small even for those — the gaps either side
 * of it have met — draws nothing, rather than a shape turned inside out.
 */
export function sectorPath(
  cx: number,
  cy: number,
  ro: number,
  ri: number,
  a0: number,
  a1: number,
  half = 0,
  corner = 0,
): string {
  const span = a1 - a0;
  if (!(span > 0) || ro <= ri) return "";
  let k = Math.max(0, Math.min(corner, (ro - ri) / 2));
  if (minSpan(ri, half, k) > span) {
    if (minSpan(ri, half, 0) >= span) return "";
    // The largest fillet that fits, by bisection: `minSpan` grows
    // monotonically with `k`, and twenty halvings is well under a
    // thousandth of a pixel.
    let lo = 0;
    let hi = k;
    for (let i = 0; i < 20; i++) {
      const mid = (lo + hi) / 2;
      if (minSpan(ri, half, mid) <= span) lo = mid;
      else hi = mid;
    }
    k = lo;
  }

  /**
   * One corner: where the fillet leaves the cut edge, and where it meets
   * the circle. `s` is +1 at the start (the sector lies clockwise of its
   * edge) and −1 at the end; `outer` picks the circle.
   */
  const corner4 = (theta: number, s: 1 | -1, outer: boolean): { edge: Pt; circle: Pt } => {
    const r = outer ? ro : ri;
    // Outward along the radius, and clockwise along the tangent.
    const ux = Math.sin(theta);
    const uy = -Math.cos(theta);
    const nx = Math.cos(theta) * s;
    const ny = Math.sin(theta) * s;
    // The fillet's centre sits `k` inside the circle (outside the hole)
    // and `k` beyond the cut edge.
    const d = outer ? r - k : r + k;
    const along = Math.sqrt(Math.max(0, d * d - (half + k) * (half + k)));
    const edge: Pt = [cx + ux * along + nx * half, cy + uy * along + ny * half];
    if (k === 0) return { edge, circle: edge };
    const ccx = ux * along + nx * (half + k);
    const ccy = uy * along + ny * (half + k);
    const circle: Pt = [cx + (ccx * r) / d, cy + (ccy * r) / d];
    return { edge, circle };
  };

  const os = corner4(a0, 1, true);
  const oe = corner4(a1, -1, true);
  const ie = corner4(a1, -1, false);
  const is = corner4(a0, 1, false);

  // Large-arc flags from the arcs' real angular extent, which is the span
  // less what the two ends took from it on that circle.
  const angleOf = (p: Pt) => Math.atan2(p[0] - cx, -(p[1] - cy));
  const extent = (from: Pt, to: Pt) => {
    let e = angleOf(to) - angleOf(from);
    while (e < 0) e += TAU;
    // A span of (almost) the whole turn measures as (almost) nothing.
    if (span > Math.PI && e < Math.PI) e += TAU;
    return e;
  };
  const outerLarge = extent(os.circle, oe.circle) > Math.PI ? 1 : 0;
  const innerLarge = extent(is.circle, ie.circle) > Math.PI ? 1 : 0;

  const P = (p: Pt) => `${f(p[0])} ${f(p[1])}`;
  const fillet = (to: Pt) => (k > 0 ? `A${f(k)} ${f(k)} 0 0 1 ${P(to)}` : "");
  return (
    `M${P(os.edge)}` +
    fillet(os.circle) +
    `A${f(ro)} ${f(ro)} 0 ${outerLarge} 1 ${P(oe.circle)}` +
    fillet(oe.edge) +
    `L${P(ie.edge)}` +
    fillet(ie.circle) +
    `A${f(ri)} ${f(ri)} 0 ${innerLarge} 0 ${P(is.circle)}` +
    fillet(is.edge) +
    "Z"
  );
}

export type RingShape = {
  /** An SVG path, filled. */
  d: string;
  /** `null` for the track. */
  index: number | null;
};

export type RingLayout = {
  /** Draw the whole track under everything (a gauge, or an empty ring). */
  track: boolean;
  shapes: RingShape[];
};

/**
 * Lay a ring out: what to draw, in what order, for these values.
 *
 * Two kinds of ring, told apart by the data rather than a flag:
 *
 *  - **A gauge** — one reading against a bigger whole (`total`). A full
 *    track, and one round-capped arc from twelve o'clock. At the whole
 *    or within half a percent of it, a closed ring: two caps meeting is
 *    the notch this module exists to get rid of.
 *  - **A breakdown** — several shares. Knife-cut sectors in order from
 *    twelve o'clock, each gap the same width end to end, and whatever
 *    the shares leave of `total` drawn as one more sector in the track's
 *    colour. No track under them: the gaps are the card, so the ring
 *    reads as one ring cut, not as tiles on a rail. A share too small to
 *    see is widened to the smallest sector that reads, taken from the
 *    largest.
 *
 * `sweep` (0–1) draws only that much of the turn — the entrance.
 */
export function layoutRing(opts: {
  values: readonly number[];
  total?: number;
  size: number;
  thickness: number;
  /** Gap between breakdown sectors, px. */
  gap: number;
  /** Corner radius of breakdown sectors, px. */
  corner: number;
  sweep?: number;
}): RingLayout {
  const { size, thickness } = opts;
  const sweep = Math.max(0, Math.min(1, opts.sweep ?? 1));
  const c = size / 2;
  const ro = c;
  const ri = Math.max(0, c - thickness);
  const mid = (ro + ri) / 2;
  const values = opts.values.map((v) => (Number.isFinite(v) ? Math.max(0, v) : 0));
  const sum = values.reduce((a, v) => a + v, 0);
  const whole = Math.max(opts.total ?? sum, sum);
  const live = values.map((v, i) => ({ v, i })).filter((s) => s.v > 0);

  if (whole <= 0 || live.length === 0) return { track: true, shapes: [] };

  const cap = thickness / 2;
  const gauge = live.length === 1 && opts.total != null && live[0].v < whole;
  if (gauge) {
    const frac = (live[0].v / whole) * sweep;
    if (frac <= 0) return { track: true, shapes: [] };
    if (frac >= 0.995) return { track: true, shapes: [{ d: annulusPath(c, c, ro, ri), index: live[0].i }] };
    // Never shorter than its own two caps: the smallest reading is a dot
    // sitting at twelve, not a sliver.
    const span = Math.max(frac * TAU, minSpan(ri, 0, cap) + 1e-6);
    return { track: true, shapes: [{ d: sectorPath(c, c, ro, ri, 0, span, 0, cap), index: live[0].i }] };
  }

  // One share that is the whole thing is a ring, not a sector with a gap
  // in it.
  if (live.length === 1 && sweep >= 1) {
    return { track: false, shapes: [{ d: annulusPath(c, c, ro, ri), index: live[0].i }] };
  }

  const half = opts.gap / 2;
  const segs: { span: number; index: number | null }[] = live.map((s) => ({
    span: (s.v / whole) * TAU * sweep,
    index: s.i,
  }));
  // What the shares leave, and what the entrance has not reached yet,
  // are the same thing: track.
  const rest = TAU - segs.reduce((a, s) => a + s.span, 0);
  if (rest > 1e-9) segs.push({ span: rest, index: null });

  // Widen a share nobody could see — "4 ports in use of 12,001" is a fact
  // the ring must show — to the least that reads: its two ends, and a
  // visible sliver between them.
  const least = minSpan(ri, half, Math.min(opts.corner, cap)) + Math.max(1.5, thickness * 0.3) / mid;
  let owed = 0;
  for (const s of segs) {
    if (s.index !== null && s.span < least) {
      owed += least - s.span;
      s.span = least;
    }
  }
  if (owed > 0) {
    const donors = segs.filter((s) => s.span > least * 2);
    const pool = donors.reduce((a, s) => a + s.span, 0);
    for (const s of donors) s.span -= (owed * s.span) / pool;
  }

  const shapes: RingShape[] = [];
  let at = 0;
  for (const s of segs) {
    const d = sectorPath(c, c, ro, ri, at, at + s.span, half, opts.corner);
    if (d) shapes.push({ d, index: s.index });
    at += s.span;
  }
  return { track: false, shapes };
}

/**
 * A round-capped gauge on a canvas: the same geometry as `layoutRing`'s
 * gauge, for a stroked arc. `lineCap` must be `"round"` for the arc and
 * the path is drawn along the ring's centre line, so each cap's
 * semicircle is set back half the stroke from the reading's ends.
 *
 * Adds nothing for no reading; a closed circle for a full one.
 */
export function gaugeArcTo(
  p: { moveTo(x: number, y: number): void; arc(x: number, y: number, r: number, s: number, e: number): void },
  x: number,
  y: number,
  r: number,
  width: number,
  frac: number,
): void {
  if (!(frac > 0)) return;
  // Canvas angles start at three o'clock.
  const top = -Math.PI / 2;
  if (frac >= 0.995) {
    p.moveTo(x + r, y);
    p.arc(x, y, r, 0, TAU);
    return;
  }
  const inset = Math.asin(Math.min(1, width / 2 / r));
  const a0 = top + inset;
  const a1 = Math.max(a0 + 1e-4, top + frac * TAU - inset);
  p.moveTo(x + r * Math.cos(a0), y + r * Math.sin(a0));
  p.arc(x, y, r, a0, a1);
}
