import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import { installCrashLog } from "./lib/crashlog";

// Before anything else can throw: a frontend error ends up in Director's
// own log file rather than in a console nobody opens (DIR-11).
installCrashLog();

// Dev-only: `npm run dev` + http://localhost:1420/?mock=1 runs the UI on an
// in-memory fake engine (src/lib/mock.ts) — no Tauri shell, no mesh. The
// guard is a compile-time constant, so production bundles never include it.
if (import.meta.env.DEV && new URLSearchParams(location.search).has("mock")) {
  const { installMock } = await import("./lib/mock");
  installMock();
  // …and land straight on a view: `?mock=1&view=jobs`. The state
  // layer already reads this key on boot (palette jumps use it); the URL
  // form is what makes a single page reloadable while working on it.
  const params = new URLSearchParams(location.search);
  const view = params.get("view");
  if (view) sessionStorage.setItem("app:view", view);
  // `&tab=roles` opens a sub-tab of that view, for the same reason `view`
  // exists: a screenshot of one panel should be reproducible from a URL.
  // Written to the key each view's own deep-link effect already reads.
  const subtab = params.get("tab");
  if (view && subtab) sessionStorage.setItem(`${view}:tab`, subtab);
  // `&open=deploy-api` opens one object inside that view — the workflow on
  // the build screen, for now. Same reason as `tab`: a screenshot of one
  // screen should be reproducible from a URL.
  const open = params.get("open");
  if (view && open) sessionStorage.setItem(`${view}:open`, open);
  // `&theme=light|dark` pins the theme for the same reason: a screenshot
  // of one theme should be reproducible from a URL. Written to the same
  // key the theme module reads, so it survives the module's own boot.
  const theme = params.get("theme");
  if (theme === "light" || theme === "dark" || theme === "system") {
    try {
      localStorage.setItem("mw:theme", theme);
      document.documentElement.setAttribute("data-theme", theme);
    } catch {
      /* storage disabled; the attribute alone still styles this load */
    }
  }
}

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
