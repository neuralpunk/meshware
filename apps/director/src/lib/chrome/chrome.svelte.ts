// What the toolbar is currently saying, and what it is currently offering.
//
// The toolbar is chrome, so App owns it — but only the screen underneath
// knows what the *current item* is and what you can do to it. A view sets
// `detail` (and, when it has better ones than the defaults, `chips`) in an
// effect and clears it on unmount; a view with page-level actions hands
// them up the same way, as a snippet the header renders.
//
// Navigation itself is NOT here. Which section you are in, which of its
// panels is open and where you have been all belong to `app` — see
// `state.svelte.ts` — because `go()` is the one function that moves any of
// them, and splitting the state from the mover across two modules buys an
// import cycle and nothing else.

import type { Snippet } from "svelte";

/** What the second crumb *is*.
 *
 *  `"id"` names a machine thing — an open workflow, a host being edited —
 *  and is set in mono, because it is a string somebody will paste. `"place"`
 *  names a panel of the current screen, and is set in the reading face: a
 *  panel is a word, not a value, and "Fleet" in a monospace is a small lie
 *  about what kind of thing it is. */
export type CrumbKind = "id" | "place";

const RAIL_OPEN_KEY = "rail:open";
let railOpenWasStored = false;

function loadRailOpen(): Set<string> {
  try {
    const raw = localStorage.getItem(RAIL_OPEN_KEY);
    if (raw == null) return new Set();
    const ids = JSON.parse(raw);
    if (!Array.isArray(ids)) return new Set();
    railOpenWasStored = true;
    return new Set(ids.filter((x): x is string => typeof x === "string"));
  } catch {
    return new Set();
  }
}

class ChromeState {
  /** The current item: the open workflow, the host being edited. A section
   *  with panels does not set this — the rail already knows which panel is
   *  open, and the toolbar reads it from there. */
  detail = $state<string | null>(null);
  /** How to set `detail` — see `CrumbKind`. */
  detailKind = $state<CrumbKind>("id");
  /** Replaces the default right-hand chips when a screen has better ones. */
  chips = $state<string[] | null>(null);
  /** The rail is collapsed to an icon strip. */
  railCollapsed = $state(false);

  /**
   * Which rail rows with children are open (2026-09-14): `view:mesh`,
   * `view:settings`, `section:work`, `section:services`.
   *
   * The rail used to show a section's rows only while you were in it, so
   * the only way to see what was under Mesh was to go there — a page
   * change to answer "what is in here". A disclosure answers it in place,
   * and the chevron on the row is also the one mark that says a row has
   * anything under it at all.
   *
   * Remembered per machine, in `localStorage`, because which sections a
   * person keeps open is a habit and not a session. Every read and write
   * is guarded: a webview with storage switched off still gets a rail that
   * works, it just forgets.
   */
  railOpen = $state<ReadonlySet<string>>(loadRailOpen());
  /** Whether `railOpen` came from storage, rather than being the default. */
  readonly railOpenStored = railOpenWasStored;

  railIsOpen(id: string): boolean {
    return this.railOpen.has(id);
  }

  /** Open or close one row, and remember it. */
  railToggle(id: string, open = !this.railOpen.has(id)): void {
    if (open === this.railOpen.has(id)) return;
    const next = new Set(this.railOpen);
    if (open) next.add(id);
    else next.delete(id);
    this.railOpen = next;
    try {
      localStorage.setItem(RAIL_OPEN_KEY, JSON.stringify([...next]));
    } catch {
      // Storage refused (private mode, a locked-down webview): the rail
      // still toggles, it just starts fresh next launch.
    }
  }

  /**
   * The current screen's own actions, rendered at the right of the toolbar.
   *
   * A snippet rather than a description of buttons: the actions are things
   * like `Export state` and `Add node`, which own drawers, disabled states
   * and async labels belonging to the view. Handing the header a list of
   * `{ label, onclick }` would have meant re-implementing all three up
   * there, one screen at a time.
   */
  actions = $state<Snippet | null>(null);

  /** Set the crumb, the chips and the kind; hand back the teardown a view's
   *  `$effect` should return. */
  set(detail: string | null, chips: string[] | null = null, kind: CrumbKind = "id"): () => void {
    this.detail = detail;
    this.detailKind = kind;
    this.chips = chips;
    return () => {
      this.detail = null;
      this.chips = null;
      this.detailKind = "id";
    };
  }

  /** Publish this view's toolbar actions; hand back the teardown. */
  setActions(actions: Snippet): () => void {
    this.actions = actions;
    return () => {
      // Only clear what we put there: two views overlapping for a frame
      // during a transition would otherwise blank the newcomer's buttons.
      if (this.actions === actions) this.actions = null;
    };
  }
}

export const chrome = new ChromeState();
