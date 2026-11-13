// Where the machines go — d3-force, run a few ticks at a time.
//
// This replaced a hand-rolled relaxation, and the reason is what the two
// produce rather than what they cost. The hand-rolled one repelled each
// machine only against the ones in the neighbouring grid cells and pulled
// every link toward one rest length. That converges, and it converges to a
// *blob*: evenly spread, evenly dense, every neighbourhood the same size.
//
// d3-force is the model everybody's picture of a force-directed graph
// actually comes from, and two of its forces are why:
//
//   · **`forceManyBody` is long-range.** Every machine pushes every other
//     one, summarised through a Barnes–Hut quadtree rather than truncated at
//     a cutoff. That is what pushes whole neighbourhoods apart from each
//     other instead of just separating neighbours, and it is what gives the
//     layout air.
//   · **`forceLink` weights each link by degree** — its strength is
//     `1 / min(deg(a), deg(b))`. A machine with many peers is not dragged
//     around by all of them at once, so busy racks sit still and sparsely
//     connected machines splay out on long links. That is the whole
//     hub-and-leaf structure, and it falls out of the model rather than
//     being drawn on.
//
// It runs **incrementally**, a few ticks per animation frame, so the fleet
// visibly finds its shape and then stops. Nothing here blocks: five thousand
// machines would be several seconds of solve done all at once, and the same
// solve spread over a couple of seconds of cooling is the thing the eye
// enjoys anyway.
import {
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  type Simulation,
} from "d3-force";
import type { MeshGraph } from "./graph";

/**
 * The one length everything else is a multiple of.
 *
 * Its absolute value does not matter — the viewport fits whatever comes out,
 * so the layout has no units a person ever sees. What matters is its ratio
 * to the charge, and that ratio is what makes a graph airy or cramped.
 */
const LINK_DISTANCE = 34;
const CHARGE = -38;
/** How firmly a free machine is drawn back to its settled seat during a
 *  drag, per unit of alpha. See the `memory` force. */
// Deliberately about the same as d3's own centring pull, so a drag feels
// like the textbook d3 drag — the springs overshoot, swing, and settle —
// and the tether only decides *where* they settle.
const MEMORY = 0.08;

type SimNode = {
  i: number;
  index?: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
  /** Set while a machine is held under the pointer. d3 honours these on
   *  every tick — the node sits exactly there and its velocity is zeroed —
   *  so pinning is a property of the simulation, not a patch over it. */
  fx?: number | null;
  fy?: number | null;
};
type SimLink = { source: SimNode; target: SimNode; e: number };

export type Layout = {
  /** Advance the simulation. Returns true while there is still work. */
  tick(times: number): boolean;
  settled(): boolean;
  /** Knock it and let it find a new arrangement. */
  reheat(alpha: number): void;
  /**
   * The drag, as Observable's force examples do it and as every hand expects
   * it: the held machine is pinned to the pointer, the simulation is kept
   * warm so its neighbours sway out of the way, and on release the pin comes
   * off and the springs pull it back to where it belongs.
   *
   * `grab` pins machine `i` at a world point and raises the simulation's
   * alpha *target* rather than its alpha — so the graph wakes gently instead
   * of jolting. `drag` moves the pin. `release` drops it and lets the alpha
   * cool back to zero, which is the spring-back.
   */
  grab(i: number, x: number, y: number): void;
  drag(i: number, x: number, y: number): void;
  release(i: number): void;
  /** True while a machine is pinned. */
  held(): boolean;
  /** True from the start of a drag until the fleet has cooled again after
   *  it — the stretch that has to play at real speed. */
  recovering(): boolean;
  /** Finish now, for a render that cannot wait — a snapshot, or a window
   *  with reduce-motion on. Time-bounded, so a huge fleet cannot hang the
   *  window. */
  runToRest(maxMs?: number): void;
  /** Copy the current positions out to the graph's own arrays. */
  writeInto(g: MeshGraph): void;
  /** Positions in the simulation's own space, to carry across a rebuild. */
  seatOf(id: string): [number, number] | undefined;
};

export function createLayout(
  g: MeshGraph,
  previous?: (id: string) => [number, number] | undefined,
): Layout {
  const nodes: SimNode[] = new Array(g.n);
  for (let i = 0; i < g.n; i++) {
    const was = previous?.(g.nodes[i].id);
    // A machine we already had keeps its seat, so a fleet that gains a member
    // adjusts around it instead of re-shuffling every machine on the screen.
    // A new one is left for d3 to place, which seeds a phyllotaxis spiral.
    nodes[i] = was
      ? { i, x: was[0], y: was[1], vx: 0, vy: 0 }
      : { i, x: Number.NaN, y: Number.NaN, vx: 0, vy: 0 };
  }
  const centring = 0.075 + Math.min(0.16, g.n / 2600);
  const links: SimLink[] = new Array(g.m);
  for (let e = 0; e < g.m; e++) {
    links[e] = { source: nodes[g.edges[e * 2]], target: nodes[g.edges[e * 2 + 1]], e };
  }

  // d3 seeds `x`/`y` itself for anything not already placed; NaN is how you
  // ask for that without also asking it to re-place the survivors.
  for (const nd of nodes) {
    if (Number.isNaN(nd.x)) {
      delete (nd as Partial<SimNode>).x;
      delete (nd as Partial<SimNode>).y;
    }
  }

  const sim: Simulation<SimNode, SimLink> = forceSimulation(nodes)
    .force(
      "charge",
      forceManyBody<SimNode>()
        .strength((d) => CHARGE * g.push[d.i])
        // Long-range is the point, but *unbounded* long-range on five
        // thousand machines is a quadtree walk nobody needs: past a dozen
        // link-lengths the contribution is a rounding error.
        .distanceMax(LINK_DISTANCE * 14),
    )
    .force(
      "link",
      // The distance varies per link and the strength does not: d3's default
      // strength is the degree weighting that gives the layout its structure,
      // and overriding it is how you get a blob back.
      forceLink<SimNode, SimLink>(links).distance((l) => LINK_DISTANCE * g.restLen[l.e]),
    )
    // Centring, but not with `forceCenter`: separate x and y pulls let the
    // fleet settle into something wider than it is tall, which is the shape
    // of the pane it has to live in. Per-machine strength keeps the result
    // from being a disc with everything the same distance out.
    // The pull has to grow with the fleet. Repulsion is a sum over every
    // other machine, so at three hundred it overwhelms a centring strength
    // that was right at six and the graph inflates into a ring with a hole
    // in the middle — structure the fleet does not have. This keeps the
    // sprawl and closes the hole.
    .force("x", forceX<SimNode>(0).strength((d) => centring * 0.38 * g.gravity[d.i]))
    .force("y", forceY<SimNode>(0).strength((d) => centring * g.gravity[d.i]))
    // Enough to stop two machines sitting on top of each other; not enough
    // to impose a grid.
    .force("collide", forceCollide<SimNode>(LINK_DISTANCE * 0.3).strength(0.7))
    // The anchor. The centring pulls above are deliberately gentle — strong
    // enough to close a hole in a big fleet, far weaker than a link — and
    // that is exactly wrong under a drag: in a small mesh where everyone is
    // everyone's peer, pull one machine and the links carry the other four
    // with it, the fleet slides across the pane as a body, and on release
    // there is nothing to spring back *to*, because a translated clique is
    // as much at rest as the original. So the machines that are not being
    // held carry no net momentum: whatever the mean velocity of the free
    // fleet is, it is taken back out, and their centre of mass stays where
    // it was. A drag then deforms the fleet — neighbours give way, the held
    // machine pulls its links taut — and letting go is a spring-back to a
    // place that still exists. Done on velocity rather than position so it
    // is exact, and so it composes with the integration d3 does after the
    // forces rather than being undone by it.
    .force("anchor", () => {
      let sx = 0;
      let sy = 0;
      let k = 0;
      for (const nd of nodes) {
        if (nd.fx != null) continue;
        sx += nd.vx;
        sy += nd.vy;
        k++;
      }
      if (!k) return;
      sx /= k;
      sy /= k;
      for (const nd of nodes) {
        if (nd.fx != null) continue;
        nd.vx -= sx;
        nd.vy -= sy;
      }
    })
    // The memory. A cooled d3 layout is not at equilibrium — it stopped
    // because its temperature ran out, not because the forces balanced — so
    // warming it for a drag lets it carry on relaxing, and the fleet comes to
    // rest somewhere *near* where it was rather than where it was. Fine for
    // eighty machines nobody can tell apart; on five it reads as the drag
    // having knocked the picture askew. So while a drag is in flight, and
    // until the fleet has cooled again, every free machine is tethered to the
    // seat it had when it last settled. Neighbours still give way — the
    // tether is a spring, not a pin — and letting go is a spring-back to the
    // exact picture you started from. A re-settle cuts the tethers, because
    // that gesture is a request for a *new* picture.
    .force("memory", (alpha) => {
      if (!tethered) return;
      const k = MEMORY * alpha;
      for (const nd of nodes) {
        if (nd.fx != null) continue;
        nd.vx += (seatX[nd.i] - nd.x) * k;
        nd.vy += (seatY[nd.i] - nd.y) * k;
      }
    })
    .velocityDecay(0.42)
    // How long it cools for. d3's default takes about three hundred ticks,
    // which is right for a fleet whose individual positions somebody is going
    // to read. Five thousand machines is a texture — nobody is checking where
    // one of them landed — and three hundred ticks of five thousand is twenty
    // seconds of "settling…". A big fleet gets a coarser answer, sooner.
    .alphaDecay(g.n > 1500 ? 0.06 : g.n > 400 ? 0.035 : 0.0228)
    .stop();

  const seats = new Map<string, [number, number]>();
  let pinned = 0;
  /** Where each machine sat when the fleet last came to rest untouched, and
   *  whether anyone is being pulled back toward it. */
  const seatX = new Float64Array(g.n);
  const seatY = new Float64Array(g.n);
  let tethered = false;
  const remember = () => {
    for (const nd of nodes) {
      seatX[nd.i] = nd.x;
      seatY[nd.i] = nd.y;
    }
  };

  // "Still work to do" has two causes: the simulation is cooling, or someone
  // is holding a machine and the target keeps it warm. d3's own `tick` moves
  // alpha toward the target, so with a target above zero alpha climbs rather
  // than decays and the loop must keep going.
  const active = () => sim.alpha() > sim.alphaMin() || sim.alphaTarget() > 0;

  return {
    tick(times) {
      if (!active()) return false;
      sim.tick(times);
      if (active()) return true;
      // Cooled with nobody holding anything: this is the picture, remember
      // it, and let the tethers go until the next drag.
      tethered = false;
      remember();
      return false;
    },
    settled: () => !active(),
    reheat(alpha) {
      tethered = false;
      sim.alpha(alpha).restart().stop();
    },
    grab(i, x, y) {
      const nd = nodes[i];
      if (!nd) return;
      // A drag that starts on a fleet still cooling has no settled picture to
      // return to; the one it is in now is the best there is.
      if (!tethered) {
        if (active()) remember();
        tethered = true;
      }
      if (nd.fx == null) pinned++;
      nd.fx = x;
      nd.fy = y;
      // 0.3 is the figure every d3 example has used since the API existed,
      // and it is right: enough that neighbours visibly give way, not so much
      // that the whole fleet starts reshuffling because one machine moved.
      sim.alphaTarget(0.3);
    },
    drag(i, x, y) {
      const nd = nodes[i];
      if (!nd) return;
      nd.fx = x;
      nd.fy = y;
    },
    release(i) {
      const nd = nodes[i];
      if (nd && nd.fx != null) {
        nd.fx = null;
        nd.fy = null;
        pinned--;
      }
      if (pinned <= 0) {
        pinned = 0;
        sim.alphaTarget(0);
      }
    },
    held: () => pinned > 0,
    recovering: () => tethered,
    runToRest(maxMs = 600) {
      // Bounded in *time*, not ticks: a tick costs what the fleet costs, and
      // the one thing this must never do is hang the window because somebody
      // pressed Share on a big mesh. A held machine is let go first, or the
      // target would keep alpha up for the whole budget.
      for (const nd of nodes) {
        nd.fx = null;
        nd.fy = null;
      }
      pinned = 0;
      sim.alphaTarget(0);
      const until = performance.now() + maxMs;
      while (sim.alpha() > sim.alphaMin() && performance.now() < until) sim.tick();
      sim.alpha(sim.alphaMin());
      tethered = false;
      remember();
    },
    writeInto(target) {
      for (let i = 0; i < target.n; i++) {
        const nd = nodes[i];
        target.x[i] = nd.x;
        target.y[i] = nd.y;
        seats.set(target.nodes[i].id, [nd.x, nd.y]);
      }
    },
    seatOf: (id) => seats.get(id),
  };
}
