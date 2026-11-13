// The screenshots the documentation and the website use, regenerated from
// the mock engine so they always show the current build and nobody's real
// machines.
//
//   npm run dev                                   # in another shell
//   node scripts/docs-shots.mjs [outdir] [only…]  # default outdir: ../../docs/img/director
//   node scripts/docs-shots.mjs --probe '<query>' # list the buttons on one screen
//   node scripts/docs-shots.mjs --dark [outdir] [only…]  # the dark theme, files named <shot>-dark.webp
//
// Each shot is one URL of the mock app (`?mock=1&view=…&tab=…&open=…`, see
// src/main.ts) plus, where a sheet has to be open, the button that opens
// it — found by its text, the way a person finds it. Viewport-sized, at 2×,
// written as WebP: what docs/img/director/ has always held.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PORT = 9448;
const APP = process.env.APP_PORT || 1420;
const W = 1361, H = 866, DPR = 2;

const args = process.argv.slice(2);
const DARK = args[0] === "--dark" ? (args.shift(), true) : false;
const THEME = DARK ? "dark" : "light";
const probe = args[0] === "--probe" ? args[1] ?? "" : null;
const OUT = probe === null && args[0] && !args[0].startsWith("-") && args[0].includes("/")
  ? resolve(args.shift())
  : resolve(HERE, "../../../docs/img/director");
const ONLY = probe === null ? args : [];

// One profile per run: back-to-back runs (light, then dark) shared one,
// and the second deleted it while the first run's Chrome was still
// writing to it (ENOTEMPTY on CI). Removed on exit where Chrome allows.
const profile = `/tmp/docs-shots-profile-${PORT}-${process.pid}`;
rmSync(profile, { recursive: true, force: true });
process.on("exit", () => {
  try { rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 }); } catch {}
});
const chrome = spawn("google-chrome", [
  "--headless=new", "--disable-gpu", "--no-sandbox", "--hide-scrollbars",
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  `--window-size=${W},${H}`, "about:blank",
], { stdio: "ignore" });
const done = () => {
  try { ws.close(); } catch {}
  chrome.kill();
  // Chrome may still be flushing its profile as it exits.
  try { rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 }); } catch {}
};
process.on("exit", () => chrome.kill());

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
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
const evaluate = async (expr) =>
  (await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true })).result?.value;

await send("Page.enable"); await send("Runtime.enable");
await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: DPR, mobile: false });
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: THEME }] });

async function load(query) {
  // A fresh document per shot: sessionStorage carries the deep link, and a
  // previous shot's open sheet must not leak into the next.
  await send("Page.navigate", { url: "about:blank" });
  await sleep(150);
  await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1&theme=${THEME}&${query}` });
  await sleep(3200);
}
const click = async (text, sel = "button, [role=button], a, [role=tab], [role=menuitem]") => {
  const r = await evaluate('(() => { const el = [...document.querySelectorAll(' + JSON.stringify(sel) + ')]' +
    '.filter(e => e.offsetParent !== null)' +
    '.find(e => e.textContent.replace(/\\s+/g, " ").trim().startsWith(' + JSON.stringify(text) + '));' +
    'if (!el) return "MISS"; el.click(); return "ok"; })()');
  if (r !== "ok") console.warn(`  ! no button starting "${text}"`);
  await sleep(900);
};

// Bring the panel a click opened into view: Access opens its forms inline,
// below whatever is waiting for an answer.
const reveal = async (heading) => {
  await evaluate('(() => { const h = [...document.querySelectorAll("main h2, main h3, main strong, main span")]' +
    '.filter(e => e.offsetParent !== null)' +
    '.find(e => e.textContent.trim().startsWith(' + JSON.stringify(heading) + ') && e.closest("button") === null);' +
    'h?.scrollIntoView({ block: "start" }); })()');
  await sleep(500);
};

if (probe !== null) {
  await load(probe);
  const sel = process.env.PROBE_SEL || "button, [role=button], [role=tab]";
  const texts = await evaluate("[...document.querySelectorAll(" + JSON.stringify(sel) + ")]" +
    '.filter(e => e.offsetParent !== null).map(e => e.textContent.replace(/\\s+/g, " ").trim()).filter(Boolean)');
  console.log(JSON.stringify([...new Set(texts)], null, 1));
  done();
  process.exit(0);
}

// [file name, url query, then what to do once it has loaded]
const SHOTS = [
  ["first-run", "wizard", null],
  ["mesh-fleet", "view=mesh&tab=nodes", null],
  ["mesh-topology", "view=mesh&tab=topology", async () => sleep(1200)],
  ["mesh-placement", "view=mesh&tab=placement", null],
  ["mesh-capacity", "view=mesh&tab=capacity", null],
  ["mesh-updates", "view=mesh&tab=updates", null],
  ["add-machines", "view=mesh&tab=nodes", async () => click("Add machines")],
  ["scripts", "view=library", null],
  ["new-run", "view=run", null],
  ["activity", "view=jobs", null],
  ["deployments", "view=deployments", null],
  ["deployment-new", "view=deployments", async () => click("New deployment")],
  ["deployment-open", "view=deployments&open=webapp", null],
  ["deployment-degraded", "view=deployments&open=log-shipper", null],
  ["deployment-versions", "view=deployments&open=webapp", async () => click("Versions")],
  ["schedules", "view=schedules", null],
  ["addresses", "view=registry", null],
  ["access", "view=access", null],
  ["access-grant", "view=access", async () => { await click("Grant a role"); await reveal("Grant a role"); }],
  ["access-invite", "view=access", async () => { await click("Invite someone"); await reveal("Invite someone"); }],
  ["secrets", "view=secrets", null],
  ["settings-this-peer", "view=settings&tab=this-peer", null],
];

mkdirSync(OUT, { recursive: true });
for (const [name, query, then] of SHOTS) {
  if (ONLY.length && !ONLY.includes(name)) continue;
  await load(query);
  if (then) await then();
  await sleep(400);
  const { data } = await send("Page.captureScreenshot", { format: "webp", quality: 88 });
  writeFileSync(`${OUT}/${name}${DARK ? "-dark" : ""}.webp`, Buffer.from(data, "base64"));
  console.log(name);
}
done();
console.log(`→ ${OUT}`);
