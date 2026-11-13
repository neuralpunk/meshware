// The shared mesh snapshot (L2 P2) — the live topology, composed into a
// card somebody can post.
//
// **It is not a screenshot of the app.** A screenshot carries the window
// chrome, the inspector, whichever tab happened to be open and — the part
// that matters — every identifier on screen. This composes a card from the
// diagram and a handful of facts, and D-L2.3 fixes which facts: the mesh's
// name, the totals, uptime and the job count. No node ids, no user names,
// no addresses, no mesh id. A picture of your fleet should be safe to put
// in a tweet.
//
// Node *names* stay, because they are the topology: a ring of unlabelled
// circles is not a picture of anything. D-L2.3 enumerates what must go and
// they are not in it.
//
// ## Where the diagram comes from
//
// The caller hands in a *painter* rather than an element: a function that
// draws the topology into a detached canvas at whatever size we ask for.
// That is not indirection for its own sake. This card is a dark artefact
// whatever theme the window is in, and the painter is called inside
// `withDarkTokens` — so the diagram is re-drawn against the dark palette
// instead of being recoloured after the fact.
//
// It also retired about a hundred lines. While the topology was an SVG, an
// export had to deep-clone it and write every computed paint value onto the
// clone, because `fill="var(--accent)"` resolves to nothing in a detached
// document that has no `:root` — the export came out blank and *looked like
// it had worked*. A canvas has no such trap: it is already pixels.

/** What the card says, beyond the diagram itself. */
export type SnapshotFacts = {
  meshName: string;
  /** `Totals::strip()`, verbatim — never re-composed here. */
  strip: string;
  coverage: string | null;
  uptimeSecs: number;
  /** The build that drew it. Small, in the corner; a PNG used as evidence
   *  in a support thread is worth more with one. */
  version: string;
};

/** Logical size of the card. Rasterised at 2× for a legible 2400px file. */
const W = 1200;
const H = 840;
const SCALE = 2;
const PAD = 48;
/** The topology panel, inside the padding. */
const PANEL = { x: PAD, y: 152, w: W - PAD * 2, h: 496 };

/**
 * Read the dark palette while `run` executes, then put the app's theme
 * back — without ever painting in between.
 *
 * The card is a dark artefact by design (the topology on a dark ground),
 * and the topology's colours come from whichever theme is in force. Rather
 * than duplicating the dark palette in TypeScript — a second copy of the
 * tokens, drifting from the first — the export borrows the real one for
 * the length of one synchronous block. The browser paints between tasks,
 * not inside one, so nothing flashes.
 */
function withDarkTokens<T>(run: () => T): T {
  const root = document.documentElement;
  const had = root.getAttribute("data-theme");
  root.setAttribute("data-theme", "dark");
  try {
    return run();
  } finally {
    if (had === null) root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", had);
  }
}

/** One resolved token value, read from `:root`. */
function token(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** Serialised SVG → a decoded `Image`, via a blob URL. Only the wordmark
 *  takes this path now; the topology arrives as a canvas. */
function rasterise(svg: SVGSVGElement): Promise<HTMLImageElement> {
  const source = new XMLSerializer().serializeToString(svg);
  const url = URL.createObjectURL(new Blob([source], { type: "image/svg+xml;charset=utf-8" }));
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      resolve(img);
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("the topology could not be rasterised"));
    };
    img.src = url;
  });
}

function roundRect(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  ctx.beginPath();
  ctx.roundRect(x, y, w, h, r);
}

/** "4d 6h" / "3h 12m" / "48s" — the same shape `state.uptime` prints. */
function uptimeWords(secs: number): string {
  if (secs >= 86400) return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
  if (secs >= 3600) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  if (secs >= 60) return `${Math.floor(secs / 60)}m`;
  return `${secs}s`;
}

/**
 * Compose the card. Returns the PNG bytes as base64, ready for
 * `snapshot_save`.
 */
export async function renderMeshSnapshot(
  paintTopology: (width: number, height: number) => HTMLCanvasElement,
  facts: SnapshotFacts,
  brandSvg?: string,
): Promise<string> {
  // One synchronous block: everything the dark palette owes this card, and
  // the diagram drawn against it. Nothing after this point touches the live
  // document, and the browser paints between tasks rather than inside one,
  // so the window never flashes dark.
  const { image, ink } = withDarkTokens(() => ({
    image: paintTopology(Math.round(PANEL.w * SCALE), Math.round(PANEL.h * SCALE)),
    ink: {
      card: token("--bg-canvas") || "#0e1113",
      panel: token("--surface-graph") || "#0b0e10",
      edge: token("--edge") || "#242a2e",
      dots: token("--graph-dots") || "#1b2126",
      title: token("--ink-1") || "#e4e8ec",
      body: token("--ink-2") || "#c8cfd5",
      muted: token("--ink-muted") || "#8b959d",
      faint: token("--ink-faint") || "#6f7981",
      accent: token("--accent-bright") || "#64e4ca",
    },
  }));

  const brand = brandSvg ? await rasterise(brandFrom(brandSvg)) : null;

  const canvas = document.createElement("canvas");
  canvas.width = W * SCALE;
  canvas.height = H * SCALE;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("this window has no 2D canvas");
  ctx.scale(SCALE, SCALE);

  // -- the ground ----------------------------------------------------------
  ctx.fillStyle = ink.card;
  ctx.fillRect(0, 0, W, H);

  const sans =
    '"Geist Variable", "Inter Variable", ui-sans-serif, system-ui, -apple-system, sans-serif';

  // -- the name ------------------------------------------------------------
  ctx.textBaseline = "alphabetic";
  ctx.fillStyle = ink.title;
  ctx.font = `600 38px ${sans}`;
  ctx.fillText(facts.meshName, PAD, PAD + 44);

  ctx.fillStyle = ink.muted;
  ctx.font = `400 16px ${sans}`;
  ctx.fillText("a meshware mesh", PAD, PAD + 74);

  // -- the wordmark, top right --------------------------------------------
  if (brand) {
    const size = 34;
    ctx.drawImage(brand, W - PAD - size, PAD + 12, size, size);
  }

  // -- the topology --------------------------------------------------------
  ctx.save();
  ctx.fillStyle = ink.panel;
  roundRect(ctx, PANEL.x, PANEL.y, PANEL.w, PANEL.h, 14);
  ctx.fill();
  ctx.strokeStyle = ink.edge;
  ctx.lineWidth = 1;
  ctx.stroke();
  ctx.clip();

  // The one decorative element the design pass allows, and the same one the
  // screen uses: the mesh dot-field. Nothing new is invented here.
  ctx.fillStyle = ink.dots;
  for (let y = PANEL.y + 10; y < PANEL.y + PANEL.h; y += 20) {
    for (let x = PANEL.x + 10; x < PANEL.x + PANEL.w; x += 20) {
      ctx.fillRect(x, y, 1.3, 1.3);
    }
  }

  // Contain, never cover: a cropped topology is a mesh with machines
  // missing from it.
  const fit = Math.min((PANEL.w - 48) / image.width, (PANEL.h - 48) / image.height);
  const dw = image.width * fit;
  const dh = image.height * fit;
  ctx.drawImage(
    image,
    PANEL.x + (PANEL.w - dw) / 2,
    PANEL.y + (PANEL.h - dh) / 2,
    dw,
    dh,
  );
  ctx.restore();

  // -- the totals ----------------------------------------------------------
  let y = PANEL.y + PANEL.h + 52;
  ctx.fillStyle = ink.body;
  ctx.font = `500 22px ${sans}`;
  ctx.fillText(facts.strip, PAD, y);

  if (facts.coverage) {
    y += 30;
    ctx.fillStyle = ink.faint;
    ctx.font = `400 15px ${sans}`;
    ctx.fillText(facts.coverage, PAD, y);
  }

  // -- the footer ----------------------------------------------------------
  //
  // "no control plane" is the payload. Every share carries the product's
  // one sentence in a user's voice rather than a marketer's, which is worth
  // more than anything else that could occupy this line.
  const base = H - PAD - 6;
  ctx.fillStyle = ink.muted;
  ctx.font = `400 15px ${sans}`;
  ctx.fillText(`up ${uptimeWords(facts.uptimeSecs)}`, PAD, base);

  ctx.fillStyle = ink.faint;
  ctx.font = `400 13px ${sans}`;
  ctx.fillText(`meshnode ${facts.version}`, PAD, base + 22);

  ctx.fillStyle = ink.accent;
  ctx.font = `500 19px ${sans}`;
  ctx.textAlign = "right";
  ctx.fillText("no control plane", W - PAD, base);
  ctx.textAlign = "left";

  const blob = await new Promise<Blob | null>((resolve) =>
    canvas.toBlob(resolve, "image/png"),
  );
  if (!blob) throw new Error("the snapshot could not be encoded");
  return base64(await blob.arrayBuffer());
}

/** The app icon, parsed out of its raw source so it can be rasterised. */
function brandFrom(raw: string): SVGSVGElement {
  const doc = new DOMParser().parseFromString(raw, "image/svg+xml");
  const svg = doc.documentElement as unknown as SVGSVGElement;
  svg.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  if (!svg.getAttribute("width")) svg.setAttribute("width", "128");
  if (!svg.getAttribute("height")) svg.setAttribute("height", "128");
  return svg;
}

/** Bytes → base64, in chunks: `String.fromCharCode(...bytes)` on a
 *  half-megabyte buffer overflows the argument stack. */
function base64(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  const CHUNK = 0x8000;
  let out = "";
  for (let i = 0; i < bytes.length; i += CHUNK) {
    out += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(out);
}
