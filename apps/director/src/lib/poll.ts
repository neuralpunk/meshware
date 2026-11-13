// One polling primitive for every view-local ticker (M-24).
//
// The core tick in `state.svelte.ts` has always paused on a hidden window,
// with the battery argument written down beside it. Nine view-local
// `setInterval`s did not, so minimising Director on Nodes or Settings kept
// nineteen-odd IPC calls per two seconds flowing into the engine — each one
// a signed request that builds a whole collection under the state read
// lock — to keep pixels fresh behind another application. The central
// loop's rationale was defeated by the views it was written for.
//
// Three properties, none of which the nine copies had all of:
//
//   · **Hidden windows do not poll.** The tick is skipped, not stopped, so
//     nothing has to be restarted; `visibilitychange` fires one immediate
//     catch-up so the pause is invisible on the way back.
//   · **One request at a time.** A slow engine cannot make a view stack
//     requests on itself — the guard `refreshExtended` already had and the
//     view-local pollers did not.
//   · **Errors do not stop the clock.** A mesh mid-restart is a transient;
//     every one of these sites already swallowed it, and swallowing it here
//     means each caller stops re-deriving that rule.

/** Is there a document, and is anyone looking at it? */
function visible(): boolean {
  return typeof document === "undefined" || !document.hidden;
}

/**
 * Run `fn` now and every `ms` while the window is visible.
 *
 * Returns a teardown, so the whole of a caller's use is:
 *
 * ```ts
 * $effect(() => poll(load, 2000));
 * ```
 *
 * `fn` may be async; overlapping calls are suppressed rather than queued —
 * a poll that is still in flight when the next tick lands is the answer
 * that tick wanted.
 */
export function poll(fn: () => unknown, ms: number): () => void {
  let stopped = false;
  let inFlight = false;

  const run = async () => {
    if (stopped || inFlight) return;
    inFlight = true;
    try {
      await fn();
    } catch {
      // Transient (engine restarting, view unmounting mid-request); the
      // next tick retries. Callers that want to *show* a failure keep
      // their own try/catch inside `fn`.
    } finally {
      inFlight = false;
    }
  };

  const tick = () => {
    if (!visible()) return;
    void run();
  };

  // The catch-up that makes the pause invisible: come back to the window
  // and the tables are right before the next interval would have fired.
  const onVisibility = () => {
    if (visible()) void run();
  };

  void run();
  const timer = setInterval(tick, ms);
  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", onVisibility);
  }

  return () => {
    stopped = true;
    clearInterval(timer);
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", onVisibility);
    }
  };
}
