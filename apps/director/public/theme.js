// Theme, stamped before first paint.
//
// A separate file rather than an inline <script> so the window can run under
// a real Content-Security-Policy (S3): `script-src 'self'` is the whole point
// of the policy, and Tauri only nonces scripts that have an `src`. It stays
// *blocking* (no `type="module"`, no `defer`) because a module script runs
// after the first paint, and the app would flash white on every launch. On a
// dark desktop at 2am that flash is the single most unpleasant thing an ops
// tool can do to you.
//
// "system" writes no attribute — the CSS falls through to
// prefers-color-scheme. Explicit choices stamp light/dark so the toggle wins
// in both directions.
(function () {
  try {
    var t = localStorage.getItem("mw:theme");
    if (t === "light" || t === "dark") {
      document.documentElement.setAttribute("data-theme", t);
    }
  } catch (e) {
    /* private mode / storage disabled: system preference still works */
  }
})();
