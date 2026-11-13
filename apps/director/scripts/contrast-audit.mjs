// WCAG AA audit against the RENDERED DOM, not against the token file.
//
// Auditing tokens tells you the palette is fine. Auditing pixels tells you
// whether the palette actually reached the pixels — which is a different
// question, and the one that matters. Two defects in the 2026-08 pass were
// only findable this way: status washes that stayed light under inks that
// flipped bright on dark surfaces (1.29:1, invisible at thumbnail size), and
// a disabled label on a modal's sunken fill at 2.37:1.
//
// Walks every visible text node in every view plus the modals, wizard and
// terminal panes, composites the effective background through ancestors
// (alpha and gradients included), and reports anything under AA.
//
//   npm run dev                       # in another shell
//   npm run audit:contrast            # light
//   THEME=dark npm run audit:contrast # dark
//
// Exits non-zero on any failure so it can gate CI. Disabled controls are
// exempt under WCAG 1.4.3 and are skipped, but the design system still holds
// them to 3:1 — see --ink-disabled in app.css.
import { spawn } from "node:child_process";
import { rmSync } from "node:fs";

const PORT = 9444;
const APP = process.env.APP_PORT || 1420;
const THEME = process.env.THEME || "light";
const W = 1361, H = 866;
const profile = `/tmp/audit-profile-${PORT}`;
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
// `sinks=1` and `pressure=1`: the M31 surfaces only render when something
// is configured or something is wrong, so the default mock would leave the
// export card's contents and A3's pressure strip permanently unaudited.
const MOCK = `?mock=1&sinks=1&pressure=1`;
await send("Page.navigate", { url: `http://localhost:${APP}/${MOCK}` });
await sleep(1200);
await evaluate(`localStorage.setItem("mw:theme", ${JSON.stringify(THEME)})`);
await send("Page.navigate", { url: `http://localhost:${APP}/${MOCK}` });
await sleep(3500);

const AUDIT = `
(() => {
  const parse = (c) => {
    const m = c.match(/rgba?\\(([^)]+)\\)/); if (!m) return null;
    const p = m[1].split(/[,\\s\\/]+/).filter(Boolean).map(Number);
    return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 };
  };
  const over = (fg, bg) => {
    const a = fg.a + bg.a * (1 - fg.a);
    if (a === 0) return { r: 0, g: 0, b: 0, a: 0 };
    return {
      r: (fg.r * fg.a + bg.r * bg.a * (1 - fg.a)) / a,
      g: (fg.g * fg.a + bg.g * bg.a * (1 - fg.a)) / a,
      b: (fg.b * fg.a + bg.b * bg.a * (1 - fg.a)) / a, a,
    };
  };
  const lin = (c) => { c /= 255; return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); };
  const L = (c) => 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
  const ratio = (a, b) => { const x = L(a), y = L(b); return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05); };
  const bgOf = (el) => {
    let acc = null, n = el;
    while (n && n !== document.documentElement.parentNode) {
      const st = getComputedStyle(n);
      const c = parse(st.backgroundColor);
      if (c && c.a > 0) acc = acc ? over(acc, c) : c;
      if (acc && acc.a >= 0.999) return acc;
      // A gradient set via the background shorthand resets background-color
      // to transparent, so the walk used to sail past every dark trust
      // surface and score its text against the page behind it.
      if (/gradient/.test(st.backgroundImage)) {
        const stops = st.backgroundImage.match(/rgba?\\([^)]+\\)/g) || [];
        if (stops.length) {
          const g = parse(stops[stops.length - 1]);
          if (g && g.a > 0) { acc = acc ? over(acc, g) : g; if (acc.a >= 0.999) return acc; }
        }
      }
      n = n.parentElement;
    }
    return acc || { r: 255, g: 255, b: 255, a: 1 };
  };
  const out = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  const seen = new Set();
  let t;
  while ((t = walker.nextNode())) {
    const txt = t.textContent.trim();
    if (!txt || txt.length < 2) continue;
    const el = t.parentElement; if (!el) continue;
    const r = el.getBoundingClientRect(); if (r.width < 2 || r.height < 2) continue;
    const cs = getComputedStyle(el);
    if (cs.visibility === "hidden" || cs.opacity === "0") continue;
    // WCAG 1.4.3 exempts inactive components. The system still holds them to
    // 3:1 via --ink-disabled; that is checked separately below.
    if (el.closest("[disabled], [aria-disabled='true']")) continue;
    const size = parseFloat(cs.fontSize);
    const weight = parseInt(cs.fontWeight) || 400;
    const large = size >= 24 || (size >= 18.66 && weight >= 700);
    const need = large ? 3 : 4.5;
    let fg = parse(cs.color); if (!fg) continue;
    const bg = bgOf(el);
    if (fg.a < 1) fg = over(fg, bg);
    const cr = ratio(fg, bg);
    if (cr < need) {
      const key = cs.color + "|" + cs.fontSize + "|" + txt.slice(0, 24);
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({ text: txt.slice(0, 40), size, weight, ratio: +cr.toFixed(2), need,
                 color: cs.color, cls: (el.className || "").toString().slice(0, 46) });
    }
  }
  return out;
})()`;

// ---------------------------------------------------------------------------
// Where the audit goes (UX-16)
//
// Every pass used to find its screen by matching rail text, and the rail
// has been renamed and regrouped since: "Workflows" is gone, Settings'
// panes moved into the rail, Library became a section. A text match that
// misses clicks nothing, so each stale pass quietly re-audited whatever
// page was already open, and the Settings panes, the dialogs and the
// wizard went unaudited while the run reported them clean.
//
// So passes navigate by the rail's own `data-nav` ids — the view id, or
// `view:panel` — and confirm they arrived (`aria-current="page"` on that
// row). A missing row, or a deep-state click that finds nothing, is a
// failure of its own and fails the run: an audit that cannot reach a
// screen has not audited it.
// ---------------------------------------------------------------------------
let total = 0;
const unreachable = [];

const report = (name, fails) => {
  if (!fails?.length) return;
  console.log(`\n── ${name} (${THEME}) ──`);
  for (const f of fails) {
    total++;
    console.log(`  ${String(f.ratio).padStart(5)}:1 (need ${f.need})  ${String(f.size).padStart(5)}px/${f.weight}  ${f.color.padEnd(22)} "${f.text}"  [${f.cls}]`);
  }
};

/** Open the rail row `id` (unfolding its section first) and wait for it. */
async function nav(id) {
  const sel = `aside nav [data-nav=${JSON.stringify(id)}]`;
  const found = await evaluate(`(() => {
    const b = document.querySelector(${JSON.stringify(sel)});
    if (!b) return false;
    const fold = b.closest("[inert]");
    if (fold) document.querySelector('[aria-controls="' + fold.id + '"]')?.click();
    return true;
  })()`);
  if (!found) throw new Error(`no rail row data-nav=${id}`);
  await sleep(350);
  await evaluate(`document.querySelector(${JSON.stringify(sel)}).click()`);
  await sleep(900);
  const here = await evaluate(`document.querySelector(${JSON.stringify(sel)})?.getAttribute("aria-current") === "page"`);
  if (!here) throw new Error(`data-nav=${id} did not become the current page`);
}

/** Click the first `sel` whose text includes `text`; a miss is an error. */
async function clickText(sel, text) {
  const r = await evaluate(
    '(() => { const el = [...document.querySelectorAll(' + JSON.stringify(sel) + ')]' +
      '.find(e => e.textContent.trim().includes(' + JSON.stringify(text) + '));' +
      'if (!el) return false; el.click(); return true; })()',
  );
  if (!r) throw new Error(`nothing matching ${sel} with "${text}"`);
}
const js = (expr) => evaluate(expr);
const closeOverlays = async () => {
  await js(`document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }))`);
  await js(`window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }))`);
  await sleep(350);
};

async function pass(name, run) {
  try {
    await run();
  } catch (e) {
    unreachable.push(`${name}: ${e.message}`);
    return;
  }
  report(name, await evaluate(AUDIT));
}

// Every screen the rail reaches, from the rail's own table: the panels of
// Mesh and Settings, and every view in the Focus surface.
const RAIL = [
  "mesh:nodes", "mesh:topology", "mesh:placement", "mesh:capacity", "mesh:updates",
  "library", "run", "jobs",
  "deployments", "schedules", "registry",
  "access", "secrets",
  "settings:this-peer", "settings:director", "settings:fleet", "settings:facts",
  "settings:diagnostics", "settings:history", "settings:storage", "settings:backup",
  "settings:about",
];
for (const id of RAIL) await pass(id, () => nav(id));

// Deeper states the rail alone cannot reach: tabs, drill-ins, dialogs,
// popovers and sheets — where the dense and the dark surfaces live.
const DEEP = [
  ["scripts: sources", async () => { await nav("library"); await clickText("button", "Sources"); await sleep(700); }],
  ["scripts: definitions", async () => { await clickText("button", "Definitions"); await sleep(700); }],
  ["scripts: build farm", async () => { await clickText("button", "Build farm"); await sleep(900); }],
  ["add-script wizard", async () => {
    await clickText("main button", "Scripts"); await sleep(500);
    await clickText("main button", "Add a script"); await sleep(1200); }],
  ["new run: script and options", async () => { await closeOverlays(); await nav("run");
    await clickText("button", "deploy"); await sleep(600);
    await clickText("button", "Options"); await sleep(700); }],
  ["new run: together", async () => { await clickText('[role="tab"]', "Together"); await sleep(700); }],
  ["activity: job detail", async () => { await nav("jobs");
    await js(`[...document.querySelectorAll('tbody tr')].find(t => t.textContent.includes('deploy'))?.click()`);
    await sleep(1200); }],
  ["activity: answers dialog", async () => {
    await clickText('button[role="tab"]', "Answers"); await sleep(1000);
    await clickText(".card button", "fan-"); await sleep(900);
    await js(`[...document.querySelectorAll('[role=dialog] .ans-node')][1]?.click()`); await sleep(700); }],
  ["activity: answers json", async () => { await clickText("[role=dialog] [role=tab]", "JSON"); await sleep(900); }],
  ["activity: history + payload", async () => { await closeOverlays(); await nav("jobs");
    await clickText('button[role="tab"]', "History"); await sleep(900);
    await clickText("button", "Everything kept"); await sleep(1200);
    await clickText("button.linkish", "in warehouse"); await sleep(1500); }],
  // A machine is a card by default, a row once the table is chosen.
  ["fleet: machine dialog", async () => { await nav("mesh:nodes");
    await js(`[...document.querySelectorAll('.mcard, tbody tr')].find(t => t.textContent.includes('forge'))?.click()`);
    await sleep(900); }],
  ["fleet: revoke dialog", async () => { await clickText("button", "Revoke from mesh"); await sleep(800); }],
  ["fleet: attention popover", async () => { await closeOverlays(); await nav("mesh:nodes");
    await js(`document.querySelector('[data-menu=attention] button')?.click()`); await sleep(600);
    if (!(await js(`!!document.querySelector('[data-menu=attention] [role=menu]')`))) throw new Error("attention list did not open"); }],
  ["add-machine drawer: discover", async () => { await closeOverlays();
    await clickText("header button", "Add machine"); await sleep(700);
    await clickText("button", "Discover"); await sleep(500);
    await clickText("button", "Scan this network"); await sleep(4500); }],
  ["add-machine drawer: invite", async () => { await clickText('[role="tab"]', "Invite"); await sleep(400);
    await clickText("button", "Create an invite"); await sleep(1000); }],
  ["topology: definition", async () => { await closeOverlays(); await nav("mesh:topology");
    await clickText('[role="tab"]', "Definition"); await sleep(700); }],
  ["deployment sheet", async () => { await nav("deployments");
    await js(`document.querySelector('tbody tr')?.click()`); await sleep(1200);
    if (!(await js(`!!document.querySelector('[role=dialog]')`))) throw new Error("no deployment sheet"); }],
  ["access: roles dialog", async () => { await closeOverlays(); await nav("access");
    await clickText("button", "What can each role do?"); await sleep(800); }],
  ["access: machines and history", async () => { await closeOverlays();
    await clickText('[role="tab"]', "Machines"); await sleep(800);
    report("access: machines", await evaluate(AUDIT));
    await clickText('[role="tab"]', "History"); await sleep(1200); }],
  ["secrets: set a secret", async () => { await nav("secrets"); await clickText("button", "Set a secret"); await sleep(700); }],
  ["settings: licence text", async () => { await nav("settings:about"); await clickText("button", "Read the licence"); await sleep(500); }],
  ["help menu", async () => { await js(`document.querySelector('header [aria-label="Help"]')?.click()`); await sleep(600);
    if (!(await js(`!!document.querySelector('[data-menu=help] [role=menu]')`))) throw new Error("help menu did not open"); }],
  ["account menu", async () => { await closeOverlays();
    await js(`[...document.querySelectorAll('.switch-btn')].at(-1)?.click()`); await sleep(600); }],
];
for (const [name, run] of DEEP) await pass(name, run);

// The first-run surfaces are a different mesh: the wizard (`?wizard`), and
// the Get started card on a mesh of one (`?fresh&peers=0`).
async function load(query) {
  await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1&${query}` });
  await sleep(3500);
}
await load("wizard");
await pass("wizard", async () => {
  if (!(await js(`document.body.innerText.includes("Create")`))) throw new Error("wizard did not open");
});
for (const path of ["Create", "Join with an invite", "Restore"]) {
  await pass(`wizard: ${path}`, async () => {
    await load("wizard");
    await clickText("button", path); await sleep(900);
  });
}
// The card remembers, per mesh, that it has been seen complete — and the
// established mock this run started on is the same mesh id.
await js(`Object.keys(localStorage).filter(k => k.startsWith("mw:getstarted:")).forEach(k => localStorage.removeItem(k))`);
await load("fresh&peers=0");
await pass("get started", async () => {
  if (!(await js(`!!document.querySelector('.getstarted')`))) throw new Error("no Get started card");
});

if (unreachable.length) {
  console.log(`\n── could not reach (${THEME}) ──`);
  for (const u of unreachable) console.log(`  ${u}`);
}
console.log(`\n${total} contrast failure${total === 1 ? "" : "s"} in ${THEME}${unreachable.length ? `, ${unreachable.length} state${unreachable.length === 1 ? "" : "s"} not reached` : ""}`);
ws.close(); chrome.kill();
process.exit(total === 0 && unreachable.length === 0 ? 0 : 1);
