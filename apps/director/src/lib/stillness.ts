// When the app should stop animating (H-13, M-27, M-28).
//
// Director's ambient motion — the breathing dot on every alive node and
// running badge — is *infinite* CSS animation, and `opacity` on a
// non-promoted element repaints rather than composites. A Nodes table with a
// hundred machines is a hundred independent animations, and it does not stop
// when nobody is looking:
//
// (The topology used to be the worst offender here — thirty peers meant
// ninety `stroke-dashoffset` paths repainting at 60 fps. It is a canvas now
// and paints only when something changed, so it needs none of this.)
//
//   · the window is minimised or behind another application — the core
//     poll already pauses here, and the pixels it stops updating were
//     still being repainted;
//   · a modal or the command palette is open over the topology, where a
//     `backdrop-filter: blur()` scrim then has to re-composite the whole
//     blurred region every frame because the thing behind it keeps moving.
//
// The mechanism is one class on `<html>` and one CSS rule, because the
// alternative — teaching each of the fifteen animating components to watch
// `visibilitychange` — is fifteen places to forget.
//
// **Scoped to `.mw-ambient`, never to `*`.** Pausing every animation would
// also freeze whatever enter transition happened to be in flight when a
// modal opened, leaving elements stuck half-faded. Only motion that is
// decorative and endless opts in.

let overlays = 0;
let installed = false;

function apply() {
  if (typeof document === "undefined") return;
  const still = document.hidden || overlays > 0;
  document.documentElement.classList.toggle("mw-still", still);
}

/** Start watching `visibilitychange`. Idempotent; called once from the shell. */
export function installStillness(): () => void {
  if (typeof document === "undefined" || installed) return () => {};
  installed = true;
  document.addEventListener("visibilitychange", apply);
  apply();
  return () => {
    document.removeEventListener("visibilitychange", apply);
    installed = false;
  };
}

/**
 * Mark an overlay open for as long as the returned teardown is unclaimed.
 *
 * Counted rather than boolean: a sheet can open a modal, and the second one
 * closing must not un-pause the app while the first is still up.
 *
 * ```svelte
 * $effect(() => (open ? holdStill() : undefined));
 * ```
 */
export function holdStill(): () => void {
  overlays += 1;
  apply();
  let released = false;
  return () => {
    if (released) return;
    released = true;
    overlays = Math.max(0, overlays - 1);
    apply();
  };
}
