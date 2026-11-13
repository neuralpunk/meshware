// Prove the shared mesh snapshot (L2 P2) is not blank, in both themes.
//
//   npm run dev                          # in another shell
//   node scripts/snapshot-check.mjs [light|dark] [out.png]
//
// **Why this exists rather than a unit test.** The failure this phase was
// written around is that `fill="var(--graph-hub-ink)"` resolves to nothing
// in a detached SVG document, so the export comes out blank — and it looks
// like a success, because a file appears. Nothing short of a real engine
// with a real cascade can catch that: jsdom resolves custom properties by
// not having them. So this drives the actual control in the mock Director
// under headless Chrome, keeps the PNG the app would have written, and
// reads the pixels back.
//
// Same CDP harness as `shots.mjs` and `contrast-audit.mjs`, for the same
// reason: one way of driving this app from a script.
import { spawn } from "node:child_process";
import { rmSync, writeFileSync } from "node:fs";

const PORT = 9448;
const APP = process.env.APP_PORT || 1420;
const THEME = process.argv[2] || "light";
const OUT = process.argv[3] || `/tmp/mesh-snapshot-${THEME}.png`;
const W = 1361, H = 866;
const profile = `/tmp/snapcheck-profile-${PORT}`;
rmSync(profile, { recursive: true, force: true });
const chrome = spawn("google-chrome", [
  "--headless=new", "--disable-gpu", "--no-sandbox", "--hide-scrollbars",
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  `--window-size=${W},${H}`, "about:blank",
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const die = (msg) => { console.error(`FAIL [${THEME}] ${msg}`); chrome.kill(); process.exit(1); };

async function targetUrl() {
  for (let i = 0; i < 60; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      const p = list.find((t) => t.type === "page");
      if (p?.webSocketDebuggerUrl) return p.webSocketDebuggerUrl;
    } catch {}
    await sleep(250);
  }
  throw new Error("no chrome");
}
const ws = new WebSocket(await targetUrl());
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let id = 0; const pending = new Map();
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id);
    m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); }
};
const send = (method, params = {}) =>
  new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
const evaluate = async (expr) => {
  const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 900));
  return r.result?.value;
};

await send("Page.enable"); await send("Runtime.enable");
await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: THEME }] });
// `&view=mesh&tab=topology`: the Share control lives on the Topology
// panel, and Mesh opens on Fleet since the 6c pass moved the vitals there.
await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1&theme=${THEME}&view=mesh&tab=topology` });
await sleep(4500);

// L2 P1 on screen: the spec sheet, wherever it landed. Matched on the
// leaf that holds the text rather than on a tag name — the 6c layout moved
// it from a full-width band to two right-aligned lines beside the title,
// and a check that fails when a <span> becomes a <div> is a check on the
// markup rather than on the thing it is there to prove.
const band = await evaluate(`(() => {
  const el = [...document.querySelectorAll("*")]
    .filter(d => d.children.length === 0 && /machines · /.test(d.textContent || ""))[0];
  return el ? el.textContent.trim() : "MISS";
})()`);
if (band === "MISS") die("the totals strip band is not on the Mesh view");
console.log(`[${THEME}] strip: ${band}`);

// Take what the real control hands the real save command, rather than
// reaching into the module: what ships is what is checked.
await evaluate(`(() => {
  const t = window.__TAURI_INTERNALS__;
  const inner = t.invoke;
  window.__snap = null;
  t.invoke = (cmd, args, opts) => {
    if (cmd === "snapshot_save") { window.__snap = args.pngBase64; return Promise.resolve("/tmp/checked.png"); }
    return inner(cmd, args, opts);
  };
})()`);

const clicked = await evaluate(`(() => {
  const b = [...document.querySelectorAll("button")].find(x => /Share this mesh/.test(x.textContent||""));
  if (!b) return "MISS";
  if (b.disabled) return "DISABLED";
  b.click(); return "ok";
})()`);
if (clicked !== "ok") die(`the Share control was ${clicked}`);

let b64 = null;
for (let i = 0; i < 60; i++) {
  b64 = await evaluate(`window.__snap`);
  if (b64) break;
  await sleep(250);
}
if (typeof b64 !== "string" || b64.length < 100) die("nothing reached snapshot_save");

// The app's own theme must come back exactly as it was: the export borrows
// the dark palette for one synchronous block and owes it back.
const after = await evaluate(`document.documentElement.getAttribute("data-theme")`);
if (after !== THEME) die(`the export left data-theme as ${after}, not ${THEME}`);

// Read the pixels back. A blank export — the failure mode this whole phase
// was costed around — is one flat colour where the topology should be.
const stats = await evaluate(`(async () => {
  const img = new Image();
  await new Promise((res, rej) => { img.onload = res; img.onerror = rej; img.src = "data:image/png;base64," + window.__snap; });
  const c = document.createElement("canvas");
  c.width = img.width; c.height = img.height;
  const x = c.getContext("2d");
  x.drawImage(img, 0, 0);
  // The topology panel, in the card's own coordinates, at 2x.
  const d = x.getImageData(96 * 2, 152 * 2, (1200 - 96) * 2, 496 * 2).data;
  const seen = new Set();
  for (let i = 0; i < d.length; i += 4) seen.add((d[i] << 16) | (d[i + 1] << 8) | d[i + 2]);
  return { w: img.width, h: img.height, colours: seen.size };
})()`);
if (stats.w !== 2400 || stats.h !== 1680) die(`the card is ${stats.w}×${stats.h}, not 2400×1680`);
if (stats.colours < 24) die(`the topology has ${stats.colours} colours in it — the export is blank`);

const buf = Buffer.from(b64, "base64");
writeFileSync(OUT, buf);
console.log(`[${THEME}] ok — ${stats.w}×${stats.h}, ${stats.colours} colours in the topology, ${buf.length} bytes → ${OUT}`);
chrome.kill();
process.exit(0);
