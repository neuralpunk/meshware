// Accessibility audit against the RENDERED app (DIR-12).
//
// Enterprise procurement asks for accessibility conformance, and the basics
// it checks first are the ones this walks for, on every screen the rail
// reaches plus the dialogs and first-run screens:
//
//   * **Names.** Every control a keyboard or a screen reader can land on —
//     buttons, links, fields, tabs, options, switches — has an accessible
//     name, and so does every dialog and every image. Read from Chrome's
//     own accessibility tree (CDP `Accessibility`), not guessed from
//     attributes, so it is the name a screen reader would speak.
//   * **Reach.** Nothing that responds to a click is out of the keyboard's
//     reach: a pointer-cursor element with no focusable self or ancestor
//     is reported, so a clickable row or card that only a mouse can open
//     does not ship again.
//   * **The dialog contract** (`lib/dialog.ts`), driven by keys rather than
//     read from markup: Ctrl+K opens the palette with focus in its
//     combobox, arrows move its active option, Escape closes it and puts
//     focus back; a Fleet row opens its machine from the keyboard, Tab
//     stays inside the dialog, and Escape returns focus to the row; and a
//     confirmation raised over a sheet closes alone on Escape.
//
// Colour contrast is `contrast-audit.mjs`; this is everything else.
//
//   npm run dev                       # in another shell
//   npm run audit:a11y
//
// Exits non-zero on any finding, or any screen it could not reach.
import { spawn } from "node:child_process";
import { rmSync } from "node:fs";

const PORT = 9445;
const APP = process.env.APP_PORT || 1420;
const W = 1361, H = 866;
const profile = `/tmp/a11y-profile-${PORT}`;
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
const key = async (k, code = k, modifiers = 0) => {
  const vk = { Escape: 27, Enter: 13, Tab: 9, ArrowDown: 40, ArrowUp: 38, k: 75 }[k] ?? 0;
  await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: k, code, modifiers, windowsVirtualKeyCode: vk });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, modifiers, windowsVirtualKeyCode: vk });
};

await send("Page.enable"); await send("Runtime.enable"); await send("DOM.enable"); await send("Accessibility.enable");
await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
async function load(query = "") {
  await send("Page.navigate", { url: `http://localhost:${APP}/?mock=1&sinks=1&pressure=1${query ? "&" + query : ""}` });
  await sleep(3500);
}
await load();

let total = 0;
const unreachable = [];
const report = (name, fails) => {
  if (!fails?.length) return;
  console.log(`\n── ${name} ──`);
  for (const f of fails) {
    total++;
    console.log(`  ${f}`);
  }
};

// ---------------------------------------------------------------------------
// Names, from the accessibility tree
// ---------------------------------------------------------------------------
const NAMED = new Set([
  "button", "link", "textbox", "searchbox", "combobox", "checkbox", "switch", "radio",
  "tab", "menuitem", "menuitemcheckbox", "menuitemradio", "option", "slider", "spinbutton",
  "listbox", "dialog", "alertdialog", "img",
]);

async function describe(backendNodeId) {
  try {
    const { node } = await send("DOM.describeNode", { backendNodeId });
    const attrs = {};
    for (let i = 0; i + 1 < (node.attributes ?? []).length; i += 2) attrs[node.attributes[i]] = node.attributes[i + 1];
    const cls = (attrs.class ?? "").split(/\s+/).slice(0, 4).join(".");
    return `<${node.localName}${attrs.id ? "#" + attrs.id : ""}${cls ? "." + cls : ""}>`;
  } catch {
    return "<?>";
  }
}

async function unnamed() {
  const { nodes } = await send("Accessibility.getFullAXTree");
  const out = [];
  for (const n of nodes) {
    if (n.ignored) continue;
    const role = n.role?.value;
    if (!NAMED.has(role)) continue;
    const name = String(n.name?.value ?? "").trim();
    if (name) continue;
    out.push(`unnamed ${role} ${await describe(n.backendDOMNodeId)}`);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Reach: clickable, but not by keyboard
// ---------------------------------------------------------------------------
const REACH = `
(() => {
  const focusable = (el) => {
    if (el.matches("a[href], button, input, select, textarea, summary, label, [contenteditable=true]")) return true;
    const t = el.getAttribute("tabindex");
    return t !== null && Number(t) >= 0;
  };
  const out = [];
  for (const el of document.querySelectorAll("body *")) {
    if (el.closest("[inert], [aria-hidden=true]")) continue;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    if (getComputedStyle(el).cursor !== "pointer") continue;
    // The outermost pointer element only: its children inherit the cursor.
    if (el.parentElement && getComputedStyle(el.parentElement).cursor === "pointer") continue;
    let a = el, ok = false;
    for (; a && a !== document.body; a = a.parentElement) if (focusable(a)) { ok = true; break; }
    if (ok) continue;
    // …or it contains the control that does the work (a row whose only
    // action is a button inside it).
    if (el.querySelector("a[href], button, input, select, textarea")) continue;
    const text = (el.textContent || "").trim().replace(/\\s+/g, " ").slice(0, 50);
    out.push("clickable, not reachable by keyboard: <" + el.localName + "." + [...el.classList].slice(0, 3).join(".") + "> \\"" + text + "\\"");
  }
  return out;
})()`;

async function pass(name, run) {
  try {
    await run();
  } catch (e) {
    unreachable.push(`${name}: ${e.message}`);
    return;
  }
  report(name, [...(await unnamed()), ...(await evaluate(REACH))]);
}

async function nav(target) {
  const sel = `aside nav [data-nav=${JSON.stringify(target)}]`;
  const found = await evaluate(`(() => {
    const b = document.querySelector(${JSON.stringify(sel)});
    if (!b) return false;
    const fold = b.closest("[inert]");
    if (fold) document.querySelector('[aria-controls="' + fold.id + '"]')?.click();
    return true;
  })()`);
  if (!found) throw new Error(`no rail row data-nav=${target}`);
  await sleep(350);
  await evaluate(`document.querySelector(${JSON.stringify(sel)}).click()`);
  await sleep(900);
}
async function clickText(sel, text) {
  const r = await evaluate(
    '(() => { const el = [...document.querySelectorAll(' + JSON.stringify(sel) + ')]' +
      '.find(e => e.textContent.trim().includes(' + JSON.stringify(text) + '));' +
      'if (!el) return false; el.click(); return true; })()',
  );
  if (!r) throw new Error(`nothing matching ${sel} with "${text}"`);
}
const closeOverlays = async () => {
  await key("Escape");
  await sleep(250);
  await key("Escape");
  await sleep(350);
};

const RAIL = [
  "mesh:nodes", "mesh:topology", "mesh:placement", "mesh:capacity", "mesh:updates",
  "library", "run", "jobs", "deployments", "schedules", "registry",
  "access", "secrets",
  "settings:this-peer", "settings:director", "settings:fleet", "settings:facts",
  "settings:diagnostics", "settings:history", "settings:storage", "settings:backup",
  "settings:about",
];
for (const r of RAIL) await pass(r, () => nav(r));

const DEEP = [
  // A machine is a card by default, a row once the table is chosen.
  ["fleet: machine dialog", async () => { await nav("mesh:nodes");
    await evaluate(`[...document.querySelectorAll('.mcard, tbody tr')].find(t => t.textContent.includes('forge'))?.click()`);
    await sleep(900); }],
  ["deployment sheet", async () => { await closeOverlays(); await nav("deployments");
    await evaluate(`document.querySelector('tbody tr')?.click()`); await sleep(1200); }],
  ["access: roles dialog", async () => { await closeOverlays(); await nav("access");
    await clickText("button", "What can each role do?"); await sleep(800); }],
  ["secrets: set a secret", async () => { await closeOverlays(); await nav("secrets");
    await clickText("button", "Set a secret"); await sleep(700); }],
  ["palette", async () => { await closeOverlays(); await key("k", "KeyK", 2); await sleep(600); }],
];
for (const [name, run] of DEEP) await pass(name, run);
await closeOverlays();

// ---------------------------------------------------------------------------
// The dialog contract, by keyboard
// ---------------------------------------------------------------------------
const contract = [];
const expect = (ok, what) => { if (!ok) contract.push(what); };
const active = () => evaluate(`(() => { const a = document.activeElement; return a ? (a.id || a.getAttribute("aria-label") || a.localName) : null; })()`);

await load("view=jobs");
// The palette: open, combobox focused, arrows move the active option, Escape
// closes and gives focus back.
await evaluate(`document.querySelector("main button")?.focus()`);
const before = await active();
await key("k", "KeyK", 2);
await sleep(500);
expect(await evaluate(`document.activeElement?.getAttribute("role") === "combobox"`), "palette: focus is not in its combobox");
const opt0 = await evaluate(`document.activeElement?.getAttribute("aria-activedescendant")`);
await key("ArrowDown");
await sleep(150);
const opt1 = await evaluate(`document.activeElement?.getAttribute("aria-activedescendant")`);
expect(opt0 && opt1 && opt0 !== opt1, `palette: ArrowDown did not move the active option (${opt0} → ${opt1})`);
await key("Escape");
await sleep(500);
expect(!(await evaluate(`!!document.getElementById("palette-list")`)), "palette: Escape did not close it");
expect((await active()) === before, `palette: focus did not return (${before} → ${await active()})`);

// A Fleet machine, from the keyboard: Enter opens it, Tab stays inside,
// Escape closes it and focus goes back to where it was. A card by default,
// a row once the table is chosen (2026-09-22); the same contract for both.
await load();
await nav("mesh:nodes");
await evaluate(`[...document.querySelectorAll('.mcard, tbody tr[data-rowaction]')].find(t => t.textContent.includes('forge'))?.focus()`);
expect(await evaluate(`document.activeElement?.matches(".mcard, tr[data-rowaction]")`), "fleet: a machine does not take focus");
await key("Enter");
await sleep(900);
expect(await evaluate(`!!document.querySelector('[role=dialog][aria-modal=true]')`), "fleet: Enter on a machine did not open it");
expect(await evaluate(`!!document.activeElement?.closest('[role=dialog]')`), "fleet: focus did not move into the dialog");
for (let i = 0; i < 40; i++) await key("Tab");
await sleep(100);
expect(await evaluate(`!!document.activeElement?.closest('[role=dialog]')`), "fleet: Tab walked out of the dialog");
await key("Escape");
await sleep(700);
expect(!(await evaluate(`!!document.querySelector('[role=dialog][aria-modal=true]')`)), "fleet: Escape did not close the dialog");
expect(await evaluate(`document.activeElement?.matches(".mcard, tr[data-rowaction]") && document.activeElement.textContent.includes("forge")`), "fleet: focus did not return to the machine");
// A dialog raised from a dialog: a deployment's delete confirmation, over
// its sheet. One Escape takes down the one on top, not both.
const dialogs = () => evaluate(`document.querySelectorAll('[role=dialog][aria-modal=true]').length`);
await nav("deployments");
await evaluate(`document.querySelector('tbody tr')?.click()`);
await sleep(1200);
await evaluate(`[...document.querySelectorAll('[role=dialog] button.btn-danger')].find(b => b.textContent.trim() === "Delete")?.click()`);
await sleep(800);
const stacked = await dialogs();
expect(stacked === 2, `stacked dialogs: expected 2 open, found ${stacked}`);
await key("Escape");
await sleep(700);
expect((await dialogs()) === 1, "stacked dialogs: one Escape closed more (or less) than the top one");
expect(await evaluate(`!!document.activeElement?.closest('[role=dialog]')`), "stacked dialogs: focus did not return into the dialog underneath");
await key("Escape");
await sleep(700);
expect((await dialogs()) === 0, "stacked dialogs: the second Escape did not close the one underneath");
report("the dialog contract", contract);

// First run: the wizard, on a mesh that does not exist yet.
await load("wizard");
await pass("wizard", async () => {
  if (!(await evaluate(`document.body.innerText.includes("Create")`))) throw new Error("wizard did not open");
});

if (unreachable.length) {
  console.log(`\n── could not reach ──`);
  for (const u of unreachable) console.log(`  ${u}`);
}
console.log(`\n${total} accessibility finding${total === 1 ? "" : "s"}${unreachable.length ? `, ${unreachable.length} state${unreachable.length === 1 ? "" : "s"} not reached` : ""}`);
ws.close(); chrome.kill();
process.exit(total === 0 && unreachable.length === 0 ? 0 : 1);
