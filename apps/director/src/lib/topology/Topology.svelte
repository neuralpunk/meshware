<script lang="ts">
  // The topology canvas: one drawing, and the only one.
  //
  // This replaced an SVG diagram of about six hundred lines, and the reason
  // is not tidiness. The SVG drew one `<g>` per machine with a chip, an arc,
  // an aura, a glyph and two `<text>` nodes inside it — call it eight
  // elements a peer, plus a gradient in `<defs>` per spoke. At forty machines
  // that is fine. At two thousand it is sixteen thousand DOM nodes that
  // Svelte reconciles every poll, and the window stops answering the mouse.
  //
  // A canvas has one element in it whatever the fleet size, and the cost of a
  // frame becomes the cost of the *drawing* — which is a thing you can
  // budget. Everything below is written to that budget: typed arrays instead
  // of objects, one batched `Path2D` per line style instead of a stroke per
  // edge, colour bucketing so the whole fleet paints in a handful of fills,
  // and a dirty flag so a diagram nobody is touching costs nothing at all.
  //
  // ## What a machine looks like
  //
  // **Its colour and shape are whether it is answering.** A teal disc is a
  // live machine. An amber disc with a dashed edge is one the mesh suspects.
  // A donut — a disc with its middle knocked out — is one the owner has told
  // not to take work. A machine that has stopped answering is a *hole*: an
  // outline with nothing in it, because a grey disc is another colour of
  // present and the whole point is that it is not.
  //
  // **How hard it is working is a ring around it**, in one colour (indigo,
  // this app's hue for facts about hardware), filled clockwise with the CPU
  // reading and repainted on every poll. A fleet too dense for rings reads
  // load as the disc's intensity instead — still one hue, never a status
  // colour.
  //
  // This used to be the other way round: load owned colour, in five bands
  // from grey-blue to red, and liveness owned shape. But the busy bands
  // were amber and red — the colours of "suspect" and "failed" everywhere
  // else in the product — so a machine at 96% was a solid red disc that read
  // as broken in every demo (UX-13). Status owns colour now, as it does on
  // every other screen, and load is a quantity drawn as a quantity.
  //
  // ## What a drag does
  //
  // Take hold of a machine and it follows the pointer, pinned there while
  // the simulation stays warm underneath it, so its neighbours sway out of
  // the way. Let go and the pin comes off: the springs pull it back toward
  // where it belongs and the fleet settles again. That is d3's own drag
  // idiom — pin while held, unpin on release — and it is the gesture that
  // makes a force layout believable: a graph you can disturb and watch
  // recover is a graph you trust. Dragging the *ground* pans the view.
  //
  // ## What a click does
  //
  // At rest the dot carries liveness and load and nothing else. Selecting a machine lights
  // it and its peers, drops everything unrelated to a mute grey, names what
  // is left, and leaves the detail — every status field, in words — to the
  // inspector beside the pane. Asking the dot to carry less and the click to
  // carry the rest is the only labelling policy that survives a big fleet:
  // five thousand names do not fit, and never will.

  import { motion } from "../motion";
  import { gaugeArcTo } from "../arcs";
  import { createLayout, type Layout } from "./layout";
  import {
    buildGraph,
    fleetKey,
    loadBand,
    LOAD_BANDS,
    makeIndex,
    nearest,
    remeasure,
    type MeshGraph,
    type MeshNode,
    type PointIndex,
  } from "./graph";

  let {
    nodes,
    selectedId = null,
    showLinks = true,
    onselect,
    onstats,
  }: {
    nodes: MeshNode[];
    /** The machine the inspector is open on. Drives the whole highlight:
     *  everything that is not it or one of its peers recedes. */
    selectedId?: string | null;
    /** Whether peer links are drawn. */
    showLinks?: boolean;
    onselect?: (id: string | null) => void;
    /** Reported whenever the graph is rebuilt, so the legend can say how
     *  many links it is looking at without counting them a second time. */
    onstats?: (s: { nodes: number; links: number; complete: boolean }) => void;
  } = $props();

  const TAU = Math.PI * 2;
  const clamp = (v: number, a: number, b: number) => (v < a ? a : v > b ? b : v);

  // ---------------------------------------------------------------------------
  // Palette
  //
  // Read from the live CSS custom properties rather than hard-coded, so the
  // canvas follows the theme the way every other surface does — and so the
  // share card can re-render this component inside `withDarkTokens` and get a
  // dark diagram out of a light window without knowing that is happening.
  // ---------------------------------------------------------------------------
  const TOKENS = [
    "graph-edge-alive",
    "graph-edge-suspect",
    "graph-edge-dead",
    "graph-node-fill",
    "liveness-alive",
    "liveness-suspect",
    "liveness-dead",
    "accent",
    "hue-sky",
    "hue-plum",
    "hue-indigo",
    "hue-green",
    "ink-1",
    "ink-3",
    "ink-4",
    "ink-5",
    "ink-muted",
    "ink-faint",
    "status-warn",
    "status-error",
    "surface-graph",
  ] as const;
  type Token = (typeof TOKENS)[number];
  type Palette = Record<Token, string>;
  let pal: Palette | null = null;
  const alphaCache = new Map<string, string>();

  function palette(): Palette {
    if (pal) return pal;
    const cs = getComputedStyle(canvas ?? document.documentElement);
    const out = {} as Palette;
    for (const t of TOKENS) out[t] = cs.getPropertyValue("--" + t).trim() || "#888888";
    pal = out;
    return out;
  }
  function dropPalette() {
    pal = null;
    alphaCache.clear();
  }

  /** Alpha on top of a token, whatever shape the token is in — the light ramp
   *  is hex and the dark one is already `rgba()`, and a canvas needs one
   *  string either way. Memoised, because building colour strings inside a
   *  per-frame loop is the cost nobody sees coming. */
  function A(color: string, a: number): string {
    if (a >= 1 && color.startsWith("#")) return color;
    const key = color + "@" + a;
    const hit = alphaCache.get(key);
    if (hit) return hit;
    const [r, g, b, base] = rgba(color);
    const out = `rgba(${r},${g},${b},${base * a})`;
    alphaCache.set(key, out);
    return out;
  }

  /** `#abc`, `#aabbcc`, `rgb(…)` and `rgba(…)` → channels plus alpha. */
  function rgba(color: string): [number, number, number, number] {
    if (color.startsWith("#")) {
      const h =
        color.length === 4
          ? [color[1] + color[1], color[2] + color[2], color[3] + color[3]]
          : [color.slice(1, 3), color.slice(3, 5), color.slice(5, 7)];
      return [parseInt(h[0], 16), parseInt(h[1], 16), parseInt(h[2], 16), 1];
    }
    const p = color
      .replace(/rgba?\(|\)/g, "")
      .split(/[,/\s]+/)
      .filter(Boolean)
      .map(Number);
    return [p[0] || 0, p[1] || 0, p[2] || 0, p[3] ?? 1];
  }

  // ---------------------------------------------------------------------------
  // The graph, and the seats it produces
  // ---------------------------------------------------------------------------
  let graph = $state.raw<MeshGraph | null>(null);
  let index: PointIndex | null = null;
  /** Where the machines are *right now*. Distinct from `graph.x/y`, which is
   *  where they belong: a fleet that gains a member eases into the new
   *  seating rather than teleporting into it. */
  let cur = { x: new Float32Array(0), y: new Float32Array(0) };
  let layout: Layout | null = null;
  let builtKey = "";
  /** True once somebody has panned or zoomed. Until then a fleet that
   *  changes shape — a machine joins, a re-settle — is refitted to the pane
   *  as it settles, the way every force-graph example opens; after it, the
   *  view is theirs and we stop touching it. */
  let userMoved = false;
  /** Whether the view is following the layout *right now*. On while the
   *  opening settle runs and off once it stops — and off the moment a hand
   *  takes hold of a machine, because a view that refits itself while you
   *  drag is a graph that slides out from under your pointer. */
  let autoFit = false;
  /** Whether the "settling…" caption is earned: only for the opening solve
   *  and a deliberate re-settle, not for the sway after a drag. */
  let announceSettle = false;

  function applyGraph(next: MeshGraph) {
    const prev = graph;
    const carry = layout?.seatOf;
    graph = next;
    cur = { x: new Float32Array(next.n), y: new Float32Array(next.n) };
    // Survivors keep their seats, so a machine joining nudges the fleet
    // rather than re-shuffling it.
    layout = createLayout(next, carry);
    grab = null;
    if (!motion.on) layout.runToRest();
    layout.writeInto(next);
    remeasure(next);
    cur.x.set(next.x);
    cur.y.set(next.y);
    index = makeIndex(cur.x, cur.y, next.n);
    syncPulses();
    if (!prev) {
      fitted = false;
      userMoved = false;
    }
    autoFit = !userMoved;
    announceSettle = true;
    onstats?.({ nodes: next.n, links: next.m, complete: next.complete });
    requestDraw();
  }

  $effect(() => {
    const key = fleetKey(nodes);
    if (key === builtKey) {
      // Same machines, same tags — so the same seats. But the *readings* have
      // moved: this is the poll where CPU climbed, or a peer went suspect.
      // Swapping the array in and repainting is the whole update, and it is
      // what makes the load colours live: re-solving a layout that cannot
      // have changed is a fleet-sized cost paid for a colour. (The key covers
      // id, name and tags, so index i is still the same machine.)
      if (graph) {
        graph.nodes = nodes;
        requestDraw();
      }
      return;
    }
    builtKey = key;
    applyGraph(buildGraph(nodes));
  });

  // Selecting a machine is a repaint and nothing more — it must not throw
  // away the view somebody just panned to.
  $effect(() => {
    void selectedId;
    void showLinks;
    requestDraw();
  });


  // ---------------------------------------------------------------------------
  // Viewport
  // ---------------------------------------------------------------------------
  let canvas = $state<HTMLCanvasElement | null>(null);
  let host = $state<HTMLDivElement | null>(null);
  let ctx: CanvasRenderingContext2D | null = null;
  let W = 0;
  let H = 0;
  let dpr = 1;
  let view = { cx: 0, cy: 0, k: 1 };
  /** The zoom a fit produces, so marks can be sized relative to it — a chip
   *  should grow when you lean in, not when the pane gets wider. */
  let k0 = 1;
  let fitted = false;
  let dragging = $state(false);
  let hover = -1;
  /** What the caption says. Held as its own state rather than read off
   *  `graph`, which is deliberately not reactive — it is typed arrays and a
   *  proxy around them would cost more than the diagram. */
  let caption = $state<{ name: string; sub: string; load: string; self: boolean } | null>(null);

  const sx = (x: number) => (x - view.cx) * view.k + W / 2;
  const sy = (y: number) => (y - view.cy) * view.k + H / 2;
  const wx = (px: number) => (px - W / 2) / view.k + view.cx;
  const wy = (py: number) => (py - H / 2) / view.k + view.cy;
  const extent = () => graph?.bounds ?? { rx: 800, ry: 500, cx: 0, cy: 0 };

  export function fit() {
    if (!graph || !W) return;
    // Frame where the machines *are*, not where they were when the solve
    // last stopped — a drag or a re-settle in progress has moved them.
    remeasure(graph);
    const b = extent();
    view.k = fitZoom();
    view.cx = b.cx;
    view.cy = b.cy;
    k0 = view.k;
    fitted = true;
    userMoved = false;
    requestDraw();
  }

  export function zoomBy(f: number, atX = W / 2, atY = H / 2) {
    if (!W) return;
    userMoved = true;
    autoFit = false;
    const px = wx(atX);
    const py = wy(atY);
    view.k = clamp(view.k * f, k0 * 0.4, k0 * 60);
    view.cx = px - (atX - W / 2) / view.k;
    view.cy = py - (atY - H / 2) / view.k;
    requestDraw();
  }

  /** The share card's source: the current view, drawn into a detached canvas
   *  at card resolution, using whatever palette is live when it is called.
   *
   *  `width`/`height` are *device* pixels, and `pixelRatio` is what they are
   *  a multiple of — the card is composed at 2x, and passing that through as
   *  the device ratio is what keeps a chip the same size on the card as it
   *  is on screen. Treating the card's 2,208 px as CSS pixels is what made
   *  the first export's machines half the size they should have been. */
  export function snapshotCanvas(width = 1240, height = 700, pixelRatio = 2): HTMLCanvasElement {
    const out = document.createElement("canvas");
    out.width = width;
    out.height = height;
    const c2 = out.getContext("2d");
    if (!c2 || !graph) return out;
    // A card cannot wait for a simulation to cool, so finish it here.
    if (layout && !layout.settled()) {
      layout.runToRest();
      layout.writeInto(graph);
      remeasure(graph);
      cur.x.set(graph.x);
      cur.y.set(graph.y);
      index = makeIndex(cur.x, cur.y, graph.n);
    }
    const keep = { W, H, dpr, view: { ...view }, k0, ctx, hover };
    dropPalette();
    W = width / pixelRatio;
    H = height / pixelRatio;
    dpr = pixelRatio;
    ctx = c2;
    hover = -1;
    const b = extent();
    view = { cx: b.cx, cy: b.cy, k: fitZoom() };
    k0 = view.k;
    paint();
    W = keep.W;
    H = keep.H;
    dpr = keep.dpr;
    view = keep.view;
    k0 = keep.k0;
    ctx = keep.ctx;
    hover = keep.hover;
    dropPalette();
    requestDraw();
    return out;
  }

  // ---------------------------------------------------------------------------
  // The loop
  //
  // One frame is scheduled when something changed, and none at all when
  // nothing did. A settled diagram nobody is touching costs zero wakeups.
  // ---------------------------------------------------------------------------
  let raf = 0;
  let lastFrame = 0;
  function requestDraw() {
    if (raf) return;
    raf = requestAnimationFrame(step);
  }
  function step(now: number) {
    raf = 0;
    const dt = lastFrame ? Math.min(0.05, (now - lastFrame) / 1000) : 1 / 60;
    lastFrame = now;

    // The settle. A small fleet gets more ticks a frame so it does not dawdle;
    // a large one gets one, because one tick of five thousand machines is
    // already a frame's worth of work.
    if (layout && graph && !layout.settled()) {
      // The opening settle is hurried — several ticks a frame, so a small
      // fleet does not dawdle. A drag and its spring-back are not: they play
      // one tick a frame, which is the clock d3's own examples run on and
      // the speed at which a spring reads as a spring rather than a snap.
      const still = layout.tick(layout.recovering() ? 1 : graph.n > 1500 ? 1 : graph.n > 400 ? 2 : 4);
      layout.writeInto(graph);
      cur.x.set(graph.x);
      cur.y.set(graph.y);
      // The picker has to know where the machines are *now*: a stale index
      // during the settle, or during a drag, is a hover that names the
      // machine that used to be there.
      index = makeIndex(cur.x, cur.y, graph.n);
      // Keep it framed while it grows into the pane — but only until the
      // solve stops or the person takes the view for themselves. Bounds and
      // spacing are left alone otherwise: remeasuring under a drag makes the
      // marks breathe as the box around them changes.
      if (autoFit) {
        remeasure(graph);
        fitted = false;
      }
      if (!still) {
        remeasure(graph);
        autoFit = false;
        announceSettle = false;
      }
      requestDraw();
    }
    if (pulsing()) {
      advancePulses(dt);
      requestDraw();
    }
    paint();
  }

  $effect(() => {
    if (!canvas || !host) return;
    ctx = canvas.getContext("2d");
    const ro = new ResizeObserver(() => resize());
    ro.observe(host);
    resize();
    // The canvas is a picture of tokens; when the tokens change, so is it.
    const mq = matchMedia("(prefers-color-scheme: dark)");
    const drop = () => {
      dropPalette();
      requestDraw();
    };
    mq.addEventListener("change", drop);
    const mo = new MutationObserver(drop);
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme", "class"] });
    return () => {
      ro.disconnect();
      mo.disconnect();
      mq.removeEventListener("change", drop);
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
    };
  });

  function resize() {
    if (!canvas || !host) return;
    const r = host.getBoundingClientRect();
    if (!r.width || !r.height) return;
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    W = r.width;
    H = r.height;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    // The inspector opening resizes this pane, and a person who has zoomed
    // into a corner of the fleet should not lose it to that. Only the
    // reference zoom moves; the view stays where they put it.
    if (fitted) k0 = fitZoom();
    requestDraw();
  }

  /** The zoom a fit would produce at the current size. */
  function fitZoom(): number {
    const b = extent();
    // Bounds cover where the machines are; a machine is also a mark and two
    // lines of label, and those hang outside it. The gutter is what stops a
    // fitted view clipping the bottom row's name.
    return (Math.min(W / (2 * b.rx), H / (2 * b.ry)) || 1) * 0.88;
  }


  // ---------------------------------------------------------------------------
  // Pointer and keyboard
  // ---------------------------------------------------------------------------
  /** A press on the ground: pans if it travels, clears the selection if it
   *  does not. */
  let down: { x: number; y: number; moved: boolean } | null = null;
  /** A press on a machine: drags it if it travels, selects it if it does
   *  not. `ox`/`oy` is where on the mark it was taken hold of, so it does not
   *  snap its centre to the pointer on the first move. */
  let grab: { i: number; x: number; y: number; ox: number; oy: number; moved: boolean } | null = null;
  /** What the pointer looks like — a hand over a machine, a fist while it
   *  holds one or drags the ground, nothing special otherwise. */
  let cursor = $state<"grab" | "grabbing" | "default">("default");
  function setCursor() {
    cursor = grab || down?.moved ? "grabbing" : hover >= 0 ? "grab" : "default";
  }

  function pickAt(px: number, py: number): number {
    if (!graph || !index) return -1;
    // Forgiving on purpose: a 6px dot is a hard target for a pointer, and
    // the cost of a generous radius is picking a neighbour, not nothing.
    return nearest(index, wx(px), wy(py), Math.max(14, markR() * 1.8) / view.k);
  }

  function setHover(i: number) {
    hover = i;
    setCursor();
    if (!graph || i < 0) {
      caption = null;
      return;
    }
    const nd = graph.nodes[i];
    caption = { name: nd.name, sub: nd.sub, load: loadWord(nd), self: nd.isSelf };
  }

  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0 && e.pointerType === "mouse") return;
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
    const hit = pickAt(e.offsetX, e.offsetY);
    if (hit >= 0 && graph && layout) {
      // Take hold of the machine. It is pinned where it already is — not
      // snapped to the pointer — and the simulation is warmed so the fleet
      // starts to answer the hand before it has even moved.
      const px = wx(e.offsetX);
      const py = wy(e.offsetY);
      grab = { i: hit, x: e.offsetX, y: e.offsetY, ox: px - cur.x[hit], oy: py - cur.y[hit], moved: false };
      layout.grab(hit, cur.x[hit], cur.y[hit]);
      autoFit = false;
      setHover(hit);
    } else {
      down = { x: e.offsetX, y: e.offsetY, moved: false };
    }
    setCursor();
    requestDraw();
  }
  function onPointerMove(e: PointerEvent) {
    if (grab && layout) {
      if (!grab.moved && Math.hypot(e.offsetX - grab.x, e.offsetY - grab.y) > 3) {
        grab.moved = true;
        dragging = true;
        setCursor();
      }
      if (grab.moved) layout.drag(grab.i, wx(e.offsetX) - grab.ox, wy(e.offsetY) - grab.oy);
      requestDraw();
      return;
    }
    if (down) {
      const dx = e.offsetX - down.x;
      const dy = e.offsetY - down.y;
      if (!down.moved && Math.hypot(dx, dy) > 3) {
        down.moved = true;
        dragging = true;
        setCursor();
      }
      if (down.moved) {
        userMoved = true;
        autoFit = false;
        view.cx -= dx / view.k;
        view.cy -= dy / view.k;
        down.x = e.offsetX;
        down.y = e.offsetY;
        requestDraw();
      }
      return;
    }
    const hit = pickAt(e.offsetX, e.offsetY);
    if (hit !== hover) {
      setHover(hit);
      requestDraw();
    }
  }
  function onPointerUp(e: PointerEvent) {
    const el = e.currentTarget as Element;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
    // On a machine: a drag lets go and the springs take over; a press that
    // never travelled is a click, and selects. On the ground: a drag has
    // already panned; a press clears the selection, which is the only way
    // out of one that does not involve hunting for a close button.
    if (grab && graph && layout) {
      layout.release(grab.i);
      if (!grab.moved && e.type !== "pointercancel") onselect?.(graph.nodes[grab.i].id);
      grab = null;
    } else if (down && !down.moved && graph && e.type !== "pointercancel") {
      onselect?.(null);
    }
    down = null;
    dragging = false;
    // Wherever the pointer let go is what it is now over.
    if (e.type !== "pointercancel") setHover(pickAt(e.offsetX, e.offsetY));
    setCursor();
    requestDraw();
  }
  function onWheel(e: WheelEvent) {
    // The pane is a fixed-height workspace, not a scrolling document, so the
    // wheel means what it means on a map.
    e.preventDefault();
    zoomBy(Math.exp(-e.deltaY * (e.deltaMode === 1 ? 0.03 : 0.0016)), e.offsetX, e.offsetY);
  }
  function onLeave() {
    if (hover !== -1) {
      setHover(-1);
      requestDraw();
    }
  }
  function onKeyDown(e: KeyboardEvent) {
    // The canvas is a control, so it answers the keyboard: arrows pan, plus
    // and minus zoom, 0 fits, Escape lets go of the selection.
    const nudge = 40 / view.k;
    if (e.key.startsWith("Arrow")) {
      userMoved = true;
      autoFit = false;
    }
    if (e.key === "ArrowLeft") view.cx -= nudge;
    else if (e.key === "ArrowRight") view.cx += nudge;
    else if (e.key === "ArrowUp") view.cy -= nudge;
    else if (e.key === "ArrowDown") view.cy += nudge;
    else if (e.key === "+" || e.key === "=") zoomBy(1.25);
    else if (e.key === "-" || e.key === "_") zoomBy(1 / 1.25);
    else if (e.key === "0") fit();
    else if (e.key === "Escape" && selectedId) onselect?.(null);
    else return;
    e.preventDefault();
    requestDraw();
  }

  /** What a screen reader gets instead of the picture. A diagram whose only
   *  description is "topology" has not been described. */
  const summary = $derived.by(() => {
    const n = nodes.length;
    const gone = nodes.filter((nd) => nd.state === "dead").length;
    const quiet = nodes.filter((nd) => nd.state === "suspect").length;
    const busy = nodes.filter((nd) => (nd.cpu ?? 0) >= 85).length;
    return (
      `Peer graph of ${n} machine${n === 1 ? "" : "s"}, coloured by whether each is answering, with processor load as a ring` +
      (busy ? `, ${busy} above 85 per cent` : "") +
      (quiet ? `, ${quiet} not answering` : "") +
      (gone ? `, ${gone} dead` : "") +
      ". Click a machine to inspect it; drag one to nudge it and it springs back. Drag the ground to pan, scroll or plus and minus to zoom, 0 to fit."
    );
  });


  // ---------------------------------------------------------------------------
  // Drawing
  // ---------------------------------------------------------------------------
  const newPaths = (n: number) => Array.from({ length: n }, () => new Path2D());

  function dot(p: Path2D, x: number, y: number, r: number) {
    if (r < 1.2) p.rect(x - r, y - r, r * 2, r * 2);
    else {
      p.moveTo(x + r, y);
      p.arc(x, y, r, 0, TAU);
    }
  }
  function ring(p: Path2D, x: number, y: number, r: number) {
    p.moveTo(x + r, y);
    p.arc(x, y, r, 0, TAU);
  }

  /** The largest a machine gets, from how far apart the machines actually
   *  are. Five machines a pane apart are dots you can hit; five thousand
   *  shoulder to shoulder are grains. Both are the same drawing. */
  /** How large a fleet still gets every machine named at rest. Past this the
   *  view names this machine and whatever is under the cursor, and the rest
   *  is what a click is for. Collisions are dropped either way, so this is a
   *  statement of intent rather than the thing keeping the text legible. */
  const NAME_UP_TO = 90;

  function markGap(): number {
    return (graph?.spacing ?? 200) * view.k;
  }
  function markR(): number {
    return clamp(markGap() * 0.17, 2.4, 15);
  }
  /** A machine's own radius — the ceiling, weighted by how many peers it
   *  holds. A node that loses peers visibly thins. */
  function radiusOf(i: number, R: number): number {
    const g = graph!;
    return R * (0.42 + 0.58 * Math.pow(g.deg[i] / g.maxDeg, 0.75));
  }

  // -- load ---------------------------------------------------------------
  //
  // The ring is the reading itself; the five bands survive as words ("61% ·
  // steady") and, on a fleet too dense for rings, as the disc's intensity —
  // five steps of one hue, from a whisper to solid.
  const LOAD_ALPHA = [0.4, 0.55, 0.7, 0.85, 1];
  /** Below this radius a ring is not something anyone can read. */
  const RING_FROM = 3.5;
  function loadWord(nd: MeshNode): string {
    if (nd.state === "dead") return "no answer";
    const b = loadBand(nd.cpu);
    return b < 0 ? "no reading" : `${nd.cpu}% · ${LOAD_BANDS[b].label}`;
  }

  /** Five hues for the tag groups — carried by the *links*, not the
   *  machines. Colour on a machine means whether it is answering and only
   *  that, so a neighbourhood has to say itself some other way, and the
   *  wire between two peers is the honest place for it. */
  const GROUP_TONES: Token[] = ["accent", "hue-sky", "hue-plum", "hue-indigo", "hue-green"];
  const hueOfGroup = (P: Palette, gi: number) => P[GROUP_TONES[gi % GROUP_TONES.length]];

  function paint() {
    if (!ctx || !graph || !W) return;
    if (!fitted) fit();
    const c = ctx;
    const P = palette();
    c.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Transparent, deliberately: the pane behind paints the dot field, and
    // the diagram sits on it the way it always has.
    c.clearRect(0, 0, W, H);
    paintMesh(c, P);
  }

  function paintMesh(c: CanvasRenderingContext2D, P: Palette) {
    const g = graph!;
    const X = cur.x;
    const Y = cur.y;
    const R = markR();
    /** The load gauge's stroke, needed while the gauges are laid out:
     *  their caps are set back by half of it. */
    const ringW = Math.max(1.5, R * 0.2);
    const NH = GROUP_TONES.length;
    const NL = LOAD_ALPHA.length;

    // -- who is lit ---------------------------------------------------------
    //
    // A selection, not a hover. Hovering across a fleet flashing whole
    // neighbourhoods on and off is a strobe; a click is a decision, and it
    // holds still while you read it.
    //
    // Peers here are the machines it actually has a *drawn link* to, not its
    // whole shared-tag neighbourhood — on a big rack that would be five
    // hundred machines, which is not what anybody means by "its peers".
    const sel = selectedId ? g.nodes.findIndex((nd) => nd.id === selectedId) : -1;
    let lit: Uint8Array | null = null;
    if (sel >= 0) {
      lit = new Uint8Array(g.n);
      lit[sel] = 1;
      for (let k = g.off[sel]; k < g.off[sel + 1]; k++) lit[g.adj[k]] = 2;
    }
    const dimming = lit !== null;

    // -- links --------------------------------------------------------------
    //
    // One path per group hue for links inside a neighbourhood, one for links
    // that cross between them, one for the selected machine's own. The
    // crossing links are drawn brighter and a hair thicker than the local
    // ones, because those are the links that actually hold the mesh together
    // — a fleet whose racks only talk to themselves is several fleets.
    //
    // In a mesh of five, the links *are* the picture, so a hairline that is
    // right for twenty thousand of them is multiplied back up.
    if (showLinks) {
      const sparse = clamp(3.4 - g.m / 14, 1, 3.4);
      const stride = dragging && g.m > 9000 ? 2 : 1;
      const ep = newPaths(NH + 2);
      for (let e = 0; e < g.m; e += stride) {
        const a = g.edges[e * 2];
        const b = g.edges[e * 2 + 1];
        const hi = dimming && (lit![a] === 1 || lit![b] === 1);
        const ax = sx(X[a]);
        const ay = sy(Y[a]);
        const bx = sx(X[b]);
        const by = sy(Y[b]);
        if ((ax < 0 && bx < 0) || (ax > W && bx > W) || (ay < 0 && by < 0) || (ay > H && by > H)) continue;
        const p = ep[hi ? NH + 1 : g.group[a] === g.group[b] ? g.group[a] % NH : NH];
        p.moveTo(ax, ay);
        p.lineTo(bx, by);
      }
      const f = dimming ? 0.22 : 1;
      for (let h = 0; h < NH; h++) {
        c.strokeStyle = A(hueOfGroup(P, h), Math.min(0.55, 0.17 * sparse) * f);
        c.lineWidth = 1;
        c.stroke(ep[h]);
      }
      // Crossing links stay the brighter of the two, because those are the
      // ones holding the mesh together — but capped, or a mesh of five comes
      // out as a cage of white wire with the neighbourhoods lost inside it.
      c.strokeStyle = A(P["ink-4"], Math.min(0.7, 0.42 * sparse) * f);
      c.lineWidth = 1.15;
      c.stroke(ep[NH]);
      c.strokeStyle = A(P.accent, 0.95);
      c.lineWidth = 1.8;
      c.stroke(ep[NH + 1]);
    }

    // -- machines -----------------------------------------------------------
    //
    // Colour and shape are liveness; load is a ring (UX-13). One bucket per
    // intensity so a dense fleet still paints in a handful of fills, and all
    // the rings in two strokes however many machines are in it.
    const fills = newPaths(NL); // answering, too small for a ring: intensity is load
    const alive = new Path2D(); // answering, with a ring
    const quiet = new Path2D(); // suspect
    const loadTrack = new Path2D();
    const loadArc = new Path2D();
    const muted = new Path2D(); // outside the selection's neighbourhood
    const holes = new Path2D(); // the middle knocked out of "not taking work"
    const deadRing = new Path2D();
    const dashRing = new Path2D();
    // A hairline rim on every disc: the one-pixel edge that makes a flat
    // fill read as a thing with a surface, and costs one stroke however
    // many machines are in it.
    const rim = new Path2D();
    let selMark: { x: number; y: number; r: number } | null = null;
    /** The few marks that get light under them — the machine in hand, the
     *  one under the cursor, the one that is selected. Never the fleet. */
    const glows: { x: number; y: number; r: number; color: string; a: number }[] = [];
    let visible = 0;
    const held = grab?.moved ? grab.i : -1;

    for (let i = 0; i < g.n; i++) {
      const px = sx(X[i]);
      const py = sy(Y[i]);
      if (px < -8 || py < -8 || px > W + 8 || py > H + 8) continue;
      visible++;
      const nd = g.nodes[i];
      let r = radiusOf(i, R);
      if (i === held) r *= 1.32;
      else if (i === hover) r *= 1.16;
      if (i === sel) {
        r = Math.max(r * 1.5, 7);
        selMark = { x: px, y: py, r };
      }
      if (i === held || i === sel || i === hover) {
        const tone =
          nd.state === "dead"
            ? P["liveness-dead"]
            : nd.state === "suspect"
              ? P["liveness-suspect"]
              : P["liveness-alive"];
        glows.push({
          x: px,
          y: py,
          r,
          color: i === sel ? P.accent : tone,
          a: i === held ? 0.55 : i === sel ? 0.4 : 0.3,
        });
      }
      // A machine that has stopped answering is a hole. Not a grey disc — a
      // disc is another colour of present, and the point is that it is not.
      // It stays a hole even when something else is selected: a death is not
      // a detail you get to mute.
      if (nd.state === "dead") {
        ring(deadRing, px, py, r);
        continue;
      }
      if (dimming && !lit![i]) {
        dot(muted, px, py, r);
        continue;
      }
      const b = loadBand(nd.cpu);
      const ringed = r >= RING_FROM;
      // Answering is teal whether or not it has reported a reading yet —
      // no reading is an empty ring, not a different colour of machine.
      if (nd.state === "suspect") dot(quiet, px, py, r);
      else if (ringed) dot(alive, px, py, r);
      else dot(fills[Math.max(0, b)], px, py, r);
      if (r >= 3) ring(rim, px, py, r);
      // The load gauge: a faint full track, and the reading drawn over it
      // clockwise from twelve o'clock.
      // Same geometry as every other gauge in Director (`lib/arcs.ts`):
      // the round caps sit inside the reading, so it starts at twelve and
      // never before, and a full reading is a closed circle rather than
      // two caps meeting in a notch.
      if (ringed && nd.cpu != null) {
        const rr = r + Math.max(2.2, r * 0.3);
        ring(loadTrack, px, py, rr);
        gaugeArcTo(loadArc, px, py, rr, ringW, Math.min(1, nd.cpu / 100));
      }
      // Told not to take work: present, still reporting, but hollowed out.
      if (nd.state === "noexec") dot(holes, px, py, r * 0.46);
      // Suspect rides on the machine's own edge rather than a halo outside
      // it, so nothing on this drawing is bigger than the machine it is about.
      if (nd.state === "suspect") ring(dashRing, px, py, r);
    }

    // Light under the marks that matter, painted before the marks so the
    // disc sits on it rather than under it. A radial falloff to nothing:
    // the way a lit object sits on a dark table, not a halo ring.
    for (const g of glows) {
      const outer = g.r * 3.2;
      const grad = c.createRadialGradient(g.x, g.y, g.r * 0.5, g.x, g.y, outer);
      grad.addColorStop(0, A(g.color, g.a));
      grad.addColorStop(0.45, A(g.color, g.a * 0.35));
      grad.addColorStop(1, A(g.color, 0));
      c.fillStyle = grad;
      c.beginPath();
      c.arc(g.x, g.y, outer, 0, TAU);
      c.fill();
    }
    for (let b = 0; b < NL; b++) {
      c.fillStyle = A(P["liveness-alive"], LOAD_ALPHA[b]);
      c.fill(fills[b]);
    }
    c.fillStyle = A(P["liveness-alive"], 0.85);
    c.fill(alive);
    c.fillStyle = P["liveness-suspect"];
    c.fill(quiet);
    c.lineWidth = ringW;
    c.strokeStyle = A(P["hue-indigo"], 0.16);
    c.stroke(loadTrack);
    c.strokeStyle = A(P["hue-indigo"], 0.95);
    c.lineCap = "round";
    c.stroke(loadArc);
    c.lineCap = "butt";
    c.fillStyle = A(P["ink-faint"], 0.6);
    c.fill(muted);
    c.strokeStyle = A(P["ink-1"], 0.16);
    c.lineWidth = 1;
    c.stroke(rim);
    // The hole is the pane's own ground, which is what makes it read as a
    // hole rather than as a grey pip.
    c.fillStyle = P["surface-graph"];
    c.fill(holes);
    c.strokeStyle = A(P["liveness-dead"], 0.95);
    c.lineWidth = Math.max(1.3, R * 0.17);
    c.stroke(deadRing);
    c.lineWidth = Math.max(1.4, R * 0.2);
    c.strokeStyle = P["status-warn"];
    c.setLineDash([Math.max(2, R * 0.55), Math.max(1.8, R * 0.45)]);
    c.stroke(dashRing);
    c.setLineDash([]);
    if (selMark) {
      c.strokeStyle = P.accent;
      c.lineWidth = 2;
      c.beginPath();
      c.arc(selMark.x, selMark.y, selMark.r, 0, TAU);
      c.stroke();
    }

    if (pulses.length && !dimming) paintPulses(c, P);

    // -- names --------------------------------------------------------------
    //
    // With a selection open: that machine and its peers, and nothing else.
    // Without one: a fleet small enough to name is entirely named, and one
    // that is not gets whatever is under the cursor plus this machine, which
    // a person needs to be able to find. A canvas of overlapping text is a
    // canvas with no text on it.
    c.textAlign = "center";
    c.textBaseline = "middle";
    // Boxes already spoken for. Every name is offered a place and the ones
    // that would land on a name already drawn are dropped — a canvas of
    // overlapping text is a canvas with no text on it, and a threshold alone
    // cannot tell the difference between forty spread out and forty in a heap.
    const taken: [number, number, number, number][] = [];
    const claim = (px: number, py: number, r: number, name: string, force: boolean) => {
      c.font = `500 11px var(--font-mono, ui-monospace, monospace)`;
      const w = c.measureText(name).width / 2 + 3;
      const box: [number, number, number, number] = [px - w, py + r + 4, px + w, py + r + 28];
      if (!force) {
        for (const t of taken) {
          if (box[0] < t[2] && box[2] > t[0] && box[1] < t[3] && box[3] > t[1]) return false;
        }
      }
      taken.push(box);
      return true;
    };
    const onScreen = (px: number, py: number) => px > -60 && py > -24 && px < W + 60 && py < H + 24;

    if (sel >= 0) {
      const cx = sx(X[sel]);
      const cy = sy(Y[sel]);
      const selR = Math.max(radiusOf(sel, R) * 1.5, 7);
      // The selection's own name goes down first and unconditionally:
      // whatever else has to give way, the machine you clicked keeps its name.
      claim(cx, cy, selR, g.nodes[sel].name, true);
      label(c, P, sel, cx, cy, selR, 1);
      // Nearest peers first, so when names have to be dropped the ones that
      // survive are the ones closest to what you clicked.
      const peers: number[] = [];
      for (let i = 0; i < g.n; i++) if (lit![i] === 2) peers.push(i);
      peers.sort((a, b) => Math.hypot(X[a] - X[sel], Y[a] - Y[sel]) - Math.hypot(X[b] - X[sel], Y[b] - Y[sel]));
      for (const i of peers) {
        const px = sx(X[i]);
        const py = sy(Y[i]);
        if (!onScreen(px, py)) continue;
        // Peers sit a link-length apart, so stacking every name under its own
        // machine guarantees a pile. Push each one outward along the line
        // from the selection and they fan instead.
        const dx = px - cx;
        const dy = py - cy;
        const len = Math.hypot(dx, dy) || 1;
        const ox = (dx / len) * 9;
        const oy = (dy / len) * 9;
        const r = radiusOf(i, R);
        if (!claim(px + ox, py + oy, r, g.nodes[i].name, false)) continue;
        label(c, P, i, px, py, r, 0.92, ox, oy);
      }
    } else if (visible <= NAME_UP_TO && R >= 2.6) {
      // Best-connected first. If the pane can only hold thirty names out of
      // ninety machines, the thirty worth having are the ones the mesh leans
      // on — and this machine, which a person has to be able to find.
      const order: number[] = [];
      for (let i = 0; i < g.n; i++) if (onScreen(sx(X[i]), sy(Y[i]))) order.push(i);
      order.sort((a, b) => (g.nodes[b].isSelf ? 1 : 0) - (g.nodes[a].isSelf ? 1 : 0) || g.deg[b] - g.deg[a]);
      for (const i of order) {
        const px = sx(X[i]);
        const py = sy(Y[i]);
        const r = radiusOf(i, R);
        if (!claim(px, py, r, g.nodes[i].name, g.nodes[i].isSelf)) continue;
        label(c, P, i, px, py, r, 1);
      }
    } else {
      const self = g.nodes.findIndex((nd) => nd.isSelf);
      for (const i of [self, hover]) {
        if (i >= 0) label(c, P, i, sx(X[i]), sy(Y[i]), radiusOf(i, R), 1);
      }
    }
    if (announceSettle && layout && !layout.settled() && !layout.held()) txt(c, "settling…", P, W / 2, 18);
  }

  function label(
    c: CanvasRenderingContext2D,
    P: Palette,
    i: number,
    px: number,
    py: number,
    r: number,
    a: number,
    ox = 0,
    oy = 0,
  ) {
    px += ox;
    py += oy;
    const nd = graph!.nodes[i];
    // A halo in the ground's own colour. In a dense mesh a name inevitably
    // lands on somebody else's machine, and the choice is between a name you
    // can read there and no names at all — every map makes it this way.
    c.lineJoin = "round";
    c.strokeStyle = A(P["surface-graph"], a * 0.85);
    c.lineWidth = 3;
    c.font = `500 11px var(--font-mono, ui-monospace, monospace)`;
    c.strokeText(nd.name, px, py + r + 11);
    c.fillStyle = A(P["ink-1"], a);
    c.fillText(nd.name, px, py + r + 11);
    c.font = `400 9.5px var(--font-mono, ui-monospace, monospace)`;
    const sub = nd.isSelf ? "this machine" : nd.sub;
    c.strokeText(sub, px, py + r + 22);
    c.fillStyle = A(
      nd.isSelf ? P.accent : nd.state === "suspect" ? P["status-warn"] : nd.state === "dead" ? P["ink-muted"] : P["ink-5"],
      a,
    );
    c.fillText(sub, px, py + r + 22);
  }

  function txt(c: CanvasRenderingContext2D, s: string, P: Palette, x: number, y: number) {
    c.fillStyle = A(P["ink-4"], 0.9);
    c.font = `500 11px var(--font-mono, ui-monospace, monospace)`;
    c.textAlign = "center";
    c.fillText(s, x, y);
  }

  // ---------------------------------------------------------------------------
  /** Nudge every machine and let the relaxation find the seats again. The
   *  same gesture the old force view had, and the reason it is worth keeping
   *  is that a layout you can disturb is a layout you can believe. */
  export function resettle() {
    if (!graph || graph.n < 2 || !layout) return;
    layout.reheat(0.9);
    autoFit = !userMoved;
    announceSettle = true;
    requestDraw();
  }

  const PULSE_UP_TO = 16;
  const PULSE_CAP = 16;
  type Pulse = { e: number; t: number; dir: number; speed: number };
  let pulses: Pulse[] = [];

  function respawn(p: Pulse, g: MeshGraph) {
    for (let tries = 0; tries < 8; tries++) {
      const e = (Math.random() * g.m) | 0;
      // Nothing flows to a machine that is not answering.
      if (g.nodes[g.edges[e * 2]].state === "dead" || g.nodes[g.edges[e * 2 + 1]].state === "dead") continue;
      p.e = e;
      break;
    }
    p.t = 0;
    p.dir = Math.random() < 0.5 ? 0 : 1;
    p.speed = 0.35 + Math.random() * 0.4;
  }

  function syncPulses() {
    const g = graph;
    const want = g && motion.on && g.n <= PULSE_UP_TO && g.m > 0 ? Math.min(PULSE_CAP, g.m) : 0;
    if (pulses.length === want) return;
    pulses = [];
    for (let i = 0; i < want; i++) {
      const p: Pulse = { e: 0, t: 0, dir: 0, speed: 0.5 };
      respawn(p, g!);
      p.t = Math.random(); // stagger, or they all leave at once
      pulses.push(p);
    }
  }

  const stillness = () => document.documentElement.classList.contains("mw-still");
  const pulsing = () => pulses.length > 0 && !stillness();

  function advancePulses(dt: number) {
    const g = graph!;
    for (const p of pulses) {
      p.t += dt * p.speed;
      if (p.t >= 1) respawn(p, g);
    }
  }

  function paintPulses(c: CanvasRenderingContext2D, P: Palette) {
    const g = graph!;
    const head = new Path2D();
    const tail = new Path2D();
    const r = clamp(markR() * 0.16, 1.6, 4);
    for (const p of pulses) {
      const a = g.edges[p.e * 2];
      const b = g.edges[p.e * 2 + 1];
      const t = p.dir ? p.t : 1 - p.t;
      const back = p.dir ? Math.max(0, t - 0.16) : Math.min(1, t + 0.16);
      const hx = sx(cur.x[a] + (cur.x[b] - cur.x[a]) * t);
      const hy = sy(cur.y[a] + (cur.y[b] - cur.y[a]) * t);
      if (hx < -20 || hy < -20 || hx > W + 20 || hy > H + 20) continue;
      tail.moveTo(sx(cur.x[a] + (cur.x[b] - cur.x[a]) * back), sy(cur.y[a] + (cur.y[b] - cur.y[a]) * back));
      tail.lineTo(hx, hy);
      dot(head, hx, hy, r);
    }
    c.strokeStyle = A(P.accent, 0.5);
    c.lineWidth = Math.max(1.4, r * 0.9);
    c.lineCap = "round";
    c.stroke(tail);
    c.lineCap = "butt";
    c.fillStyle = P.accent;
    c.fill(head);
  }
</script>

<div class="relative h-full w-full" bind:this={host}>
  <!-- A canvas is already an interactive element, so it takes no role — the
       label is the whole text alternative, and it says what the drawing
       shows rather than that a drawing exists. -->
  <canvas
    bind:this={canvas}
    class="block h-full w-full"
    style="touch-action: none; cursor: {cursor};"
    tabindex="0"
    aria-label={summary}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    onpointerleave={onLeave}
    onwheel={onWheel}
    onkeydown={onKeyDown}
    ondblclick={(e) => zoomBy(1.9, e.offsetX, e.offsetY)}
  ></canvas>

  {#if caption}
    <div
      class="pointer-events-none absolute top-3 left-1/2 z-[2] -translate-x-1/2 rounded-md px-2.5 py-[5px] text-[11px] whitespace-nowrap"
      style="background: var(--surface-1); border: 1px solid var(--edge-strong); color: var(--ink-2);"
    >
      <span class="font-semibold" style="color: var(--ink-1);">{caption.name}</span>
      {#if caption.self}<span style="color: var(--accent-ink);"> · this machine</span>{/if}
      <span style="color: var(--ink-muted);"> · </span><span class="num">{caption.load}</span>
      {#if caption.sub}<span style="color: var(--ink-muted);"> · </span><span
          class="num"
          style="color: var(--ink-5);">{caption.sub}</span
        >{/if}
    </div>
  {/if}
</div>
