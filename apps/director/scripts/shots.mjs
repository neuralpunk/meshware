// Screenshot every view of the mock-engine Director for design review.
//
//   npm run dev                 # in another shell
//   node scripts/shots.mjs [outdir] [theme]
//
// Same CDP harness as contrast-audit.mjs, but instead of auditing pixels it
// keeps them: one full-page PNG per view (plus the deep states worth
// designing against), so a design pass reviews what users see rather than
// what the source implies.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";

const PORT = 9447;
const APP = process.env.APP_PORT || 1420;
const OUT = process.argv[2] || "/tmp/director-shots";
const THEME = process.argv[3] || "light";
const W = 1361, H = 866;
mkdirSync(OUT, { recursive: true });
const profile = `/tmp/shots-profile-${PORT}`;
rmSync(profile, { recursive: true, force: true });
const chrome = spawn("google-chrome", [
  "--headless=new", "--disable-gpu", "--no-sandbox", "--hide-scrollbars",
  `--remote-debugging-port=${PORT}`, `--user-data-dir=${profile}`,
  `--window-size=${W},${H}`, "about:blank",
], { stdio: "ignore" });
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
await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: THEME }] });
await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1` });
await sleep(1200);
await evaluate(`localStorage.setItem("mw:theme", ${JSON.stringify(THEME)})`);
await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1` });
await sleep(3500);

// A rail row with children only opens and closes (2026-09-14); the rows
// under it are the destinations. So `go` names a destination, and first
// opens whichever section it is folded inside — a closed section's rows
// are `inert`, and a click on them does nothing, which is the point.
const go = async (v) => {
  const find = '[...document.querySelectorAll("aside nav button")]' +
    '.find(b=>b.textContent.trim().startsWith(' + JSON.stringify(v) + '))';
  await evaluate('(() => { const b = ' + find + '; const fold = b?.closest("[inert]");' +
    ' if (fold) document.querySelector(`[aria-controls="${fold.id}"]`)?.click(); })()');
  await sleep(300);
  await evaluate(find + '?.click()');
};
const clickText = (sel, text) => evaluate(
  '(() => { const el = [...document.querySelectorAll(' + JSON.stringify(sel) + ')]' +
  '.find(e => e.textContent.trim().includes(' + JSON.stringify(text) + '));' +
  'if (!el) return "MISS"; el.click(); return "ok"; })()');

async function shot(name, fullPage = true) {
  const params = { format: "png" };
  if (fullPage) {
    const m = await send("Page.getLayoutMetrics");
    const h = Math.min(Math.ceil(m.cssContentSize.height), 4000);
    // The app owns its own scroll regions, so "full page" means growing the
    // viewport until the inner region fits.
    const sh = await evaluate(`(() => { const s = document.querySelector("main .overflow-y-auto"); return s ? s.scrollHeight + 140 : ${h}; })()`);
    await send("Emulation.setDeviceMetricsOverride", { width: W, height: Math.min(Math.max(H, sh || h), 4000), deviceScaleFactor: 1, mobile: false });
    await sleep(350);
  }
  const { data } = await send("Page.captureScreenshot", params);
  writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, "base64"));
  await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
  await sleep(200);
  console.log(name);
}

// `go` matches on the leading text of a rail button, and the rail holds
// sub-section rows as well as section rows now — so a panel of Mesh is
// reached by its own name once Mesh is open, which is exactly how a person
// reaches it.
const SHOTS = [
  ["mesh", async () => { await go("Fleet"); await sleep(1200); }],
  ["mesh-topology", async () => { await go("Topology"); await sleep(1400); }],
  ["mesh-updates", async () => { await go("Updates"); await sleep(1200); }],
  ["mesh-capacity", async () => { await go("Capacity"); await sleep(1200); }],
  ["scripts", async () => { await go("Scripts"); await sleep(900); }],
  ["run", async () => { await go("New run"); await sleep(900); }],
  ["activity", async () => { await go("Activity"); await sleep(900); }],
  ["access", async () => { await go("Access"); await sleep(900); }],
  ["secrets", async () => { await go("Secrets"); await sleep(900); }],
  ["settings", async () => { await go("This peer"); await sleep(900); }],
  ["run-options", async () => { await go("New run"); await sleep(700); await clickText("button", "Options"); await sleep(600); }],
];
for (const [name, setup] of SHOTS) {
  await setup();
  await shot(name);
}
ws.close(); chrome.kill();
console.log(`→ ${OUT}`);
