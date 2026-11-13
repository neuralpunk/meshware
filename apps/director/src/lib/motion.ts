// Motion — the one place anime.js is configured, so the whole application
// moves with the same vocabulary.
//
// Three rules this module exists to enforce:
//
//   1. **One curve, five durations.** They are the same tokens the CSS uses
//      (`--dur-*`, `--ease`), restated here because anime.js takes numbers,
//      not `var()`. A transition that is 240ms in CSS and 300ms in JS reads
//      as two different products.
//   2. **Reduced motion is honoured in JS too.** `app.css` neutralises CSS
//      animations under `prefers-reduced-motion`, which does nothing to a
//      script-driven tween. Everything here checks `motion.on` and lands on
//      the final frame immediately when it is false.
//   3. **Motion is load-bearing or it is absent.** Entrances orient (where
//      did this panel come from), state changes confirm (that number just
//      moved, that step just went green), and drags/tabs stay pinned to the
//      pointer. Nothing here decorates.
//
// anime.js v4 is ESM and tree-shakeable, so importing the four things we
// use costs about 12 KB rather than the whole engine.

import {
  animate,
  createTimeline,
  createTimer,
  stagger,
  utils,
  svg,
  spring,
  cubicBezier,
  createDrawable,
  createMotionPath,
  scrambleText,
  splitText,
} from "animejs";
import type { AnimationParams, EasingParam } from "animejs";

export {
  animate,
  createTimeline,
  createTimer,
  stagger,
  utils,
  svg,
  spring,
  cubicBezier,
  createDrawable,
  createMotionPath,
  scrambleText,
  splitText,
};
export type { Timer } from "animejs";
export type { EasingParam };

/** The duration scale, in milliseconds — mirrors `--dur-*` in app.css. */
export const DUR = {
  instant: 75,
  fast: 150,
  base: 240,
  slow: 320,
  slower: 480,
} as const;

/**
 * The application's single easing curve — mirrors `--ease`.
 *
 * A **function**, not a string. anime.js 4.5 removed the string form for
 * `cubicBezier` (along with `steps`, `irregular` and parameterised
 * `linear`): `parseEase` now logs a deprecation warning and returns `none`,
 * which is linear. Every JS-driven animation in the console was easing
 * linearly and saying so in the console on every tween. The named power
 * eases — `out(3)`, `in(2)`, `inOut(2)` — still parse from strings and are
 * used as-is throughout.
 */
const EASE = cubicBezier(0.32, 0.72, 0, 1);
/** For things that overshoot slightly on entry: dialogs, popovers, chips. */
export const EASE_POP = cubicBezier(0.22, 1.2, 0.32, 1);

const query =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-reduced-motion: reduce)")
    : null;

/**
 * Live reduced-motion state.
 *
 * Not a Svelte rune: this module is imported by plain `.ts` as well as by
 * components, and every consumer reads it at the moment it starts an
 * animation rather than rendering from it.
 */
export const motion = {
  get on(): boolean {
    return !(query?.matches ?? false);
  },
  /** Scale a duration by the current preference — 0 means "jump to the end". */
  ms(value: number): number {
    return this.on ? value : 0;
  },
};

// ---------------------------------------------------------------------------
// Actions
//
// Svelte actions rather than transitions wherever the element is not being
// added and removed: an action runs once on mount and never fights the
// framework's own lifecycle.
// ---------------------------------------------------------------------------

/**
 * Put an element back to no inline transform at all.
 *
 * Not cosmetic. A settled entrance leaves `transform: translateY(0px)` on
 * the element, and **any** transform makes that element the containing
 * block for `position: fixed` descendants — which silently teleported
 * every `Hint` popover inside an animated panel off the right of the
 * screen. Landing on the final frame means removing the property, not
 * setting it to its identity value.
 */
function rest(node: HTMLElement) {
  node.style.transform = "";
  node.style.opacity = "";
}

export type RevealOptions = {
  /** Pixels to travel on the way in. Negative moves down-to-up. */
  y?: number;
  x?: number;
  /** Start scale, for things that should feel like they popped. */
  scale?: number;
  delay?: number;
  duration?: number;
  ease?: EasingParam;
  /** Skip entirely — useful for `use:reveal={{ when: isFirstPaint }}`. */
  when?: boolean;
};

/**
 * Fade-and-rise an element in as it mounts.
 *
 * Replaces the `.fade-in` / `.pop-in` CSS classes for anything that needs a
 * delay, a stagger or a curve the class cannot express.
 */
export function reveal(node: HTMLElement, options: RevealOptions = {}) {
  const run = (o: RevealOptions) => {
    if (o.when === false) return;
    if (!motion.on) {
      rest(node);
      return;
    }
    // Built rather than spread with `undefined` holes: anime.js reads any
    // present key as a tween, so `scale: undefined` animates scale to 0.
    const params: AnimationParams = {
      opacity: [0, 1],
      translateY: [o.y ?? 6, 0],
      duration: o.duration ?? DUR.slow,
      delay: o.delay ?? 0,
      ease: o.ease ?? EASE,
      onComplete: () => rest(node),
    };
    if (o.x !== undefined) params.translateX = [o.x, 0];
    if (o.scale !== undefined) params.scale = [o.scale, 1];
    animate(node, params);
  };
  run(options);
  return {
    update(next: RevealOptions) {
      options = next;
    },
    destroy() {},
  };
}

export type StaggerOptions = {
  /** CSS selector for the children to animate. Defaults to element children. */
  selector?: string;
  /** Milliseconds between each child. */
  each?: number;
  y?: number;
  delay?: number;
  duration?: number;
  /** Re-run whenever this value changes — e.g. the current filter. */
  key?: unknown;
  /** How many children animate; the rest land at once. Default 24. */
  cap?: number;
};

/**
 * Cascade an element's children in.
 *
 * Used for lists that are *replaced* rather than appended to — a filtered
 * table, a set of cards, a tab's contents. Passing `key` re-runs it, which
 * is what makes a search feel like it re-dealt the cards instead of
 * silently swapping them.
 */
export function staggerIn(node: HTMLElement, options: StaggerOptions = {}) {
  let lastKey: unknown = Symbol("init");

  const run = (o: StaggerOptions) => {
    const all = o.selector
      ? Array.from(node.querySelectorAll<HTMLElement>(o.selector))
      : (Array.from(node.children) as HTMLElement[]);
    if (all.length === 0) return;
    // Bounded fan-out: past two dozen the cascade stops saying anything and
    // starts costing wall-clock, so the rest of a long table is simply
    // there. The cap is on *animated* rows, never on rows shown.
    const kids = all.slice(0, o.cap ?? 24);
    if (!motion.on || kids.length === 0) {
      for (const kid of all) rest(kid);
      return;
    }
    for (const kid of all.slice(kids.length)) rest(kid);
    animate(kids, {
      opacity: [0, 1],
      translateY: [o.y ?? 7, 0],
      duration: o.duration ?? DUR.slow,
      delay: stagger(o.each ?? 26, { start: o.delay ?? 0 }),
      ease: EASE,
      onComplete: () => {
        for (const kid of kids) rest(kid);
      },
    });
  };

  run(options);
  lastKey = options.key;

  return {
    update(next: StaggerOptions) {
      if (next.key !== lastKey) {
        lastKey = next.key;
        // The DOM has not been patched yet when `update` fires on a keyed
        // change, so wait one frame for the new children to exist.
        requestAnimationFrame(() => run(next));
      }
      options = next;
    },
    destroy() {},
  };
}

/**
 * A number that counts to its new value instead of jumping.
 *
 * Only for figures a person watches change — vitals, tallies, capacity.
 * Never for identifiers, and never for anything where the intermediate
 * values would read as real readings.
 */
export function countUp(
  node: HTMLElement,
  options: { value: number; format?: (n: number) => string; duration?: number },
) {
  let current = options.value;
  const render = (n: number) => {
    node.textContent = options.format ? options.format(n) : String(Math.round(n));
  };
  render(current);

  return {
    update(next: { value: number; format?: (n: number) => string; duration?: number }) {
      options = next;
      if (next.value === current) return;
      if (!motion.on) {
        current = next.value;
        render(current);
        return;
      }
      const from = { n: current };
      current = next.value;
      animate(from, {
        n: next.value,
        duration: next.duration ?? DUR.slower,
        ease: "out(3)",
        onUpdate: () => render(from.n),
      });
    },
    destroy() {},
  };
}

/**
 * Flash an element when the value behind it changes.
 *
 * The point is peripheral: a row that updated while somebody was reading a
 * different row should say so once, quietly, and then stop.
 */
export function flash(node: HTMLElement, key: unknown) {
  let last = key;
  return {
    update(next: unknown) {
      if (next === last) return;
      last = next;
      if (!motion.on) return;
      animate(node, {
        backgroundColor: ["var(--accent-wash)", "transparent"],
        duration: DUR.slower * 2,
        ease: "out(2)",
      });
    },
    destroy() {},
  };
}

/**
 * Nudge on press.
 *
 * A 40ms scale on pointer-down is the cheapest way to make a dense tool
 * feel responsive on a slow machine: the acknowledgement is local and
 * immediate, whatever the work behind the button costs.
 */
export function pressable(node: HTMLElement, scale = 0.97) {
  const down = () => {
    if (!motion.on) return;
    animate(node, { scale, duration: DUR.instant, ease: "out(2)" });
  };
  const up = () => {
    if (!motion.on) return;
    animate(node, {
      scale: 1,
      duration: DUR.base,
      ease: EASE_POP,
      onComplete: () => {
        node.style.transform = "";
      },
    });
  };
  node.addEventListener("pointerdown", down);
  node.addEventListener("pointerup", up);
  node.addEventListener("pointerleave", up);
  node.addEventListener("pointercancel", up);
  return {
    destroy() {
      node.removeEventListener("pointerdown", down);
      node.removeEventListener("pointerup", up);
      node.removeEventListener("pointerleave", up);
      node.removeEventListener("pointercancel", up);
    },
  };
}

/**
 * Grow a collapsible section open and closed.
 *
 * `height: auto` cannot be transitioned, and `max-height` guesses always
 * either clip a long section or make a short one take a beat too long.
 * Measuring the scroll height and animating to it is exact.
 */
export function collapsible(node: HTMLElement, open: boolean) {
  let last = open;

  /** The resting state, written directly rather than tweened. `height:
   *  auto` is the only correct open value — a measured pixel height goes
   *  stale the moment a field inside grows a validation line. */
  const settle = (isOpen: boolean) => {
    node.style.height = isOpen ? "auto" : "0px";
    node.style.opacity = isOpen ? "1" : "0";
    node.style.overflow = isOpen ? "" : "hidden";
    node.style.visibility = isOpen ? "" : "hidden";
  };

  settle(open);

  return {
    update(next: boolean) {
      if (next === last) return;
      last = next;
      if (!motion.on) {
        settle(next);
        return;
      }
      node.style.overflow = "hidden";
      node.style.visibility = "";
      if (next) node.style.height = "auto";
      const full = node.scrollHeight;
      node.style.height = `${next ? 0 : full}px`;
      // Hinted for the tween only, then cleared — the rule `lift()` follows
      // (L-22). `Fold.svelte` used to carry a permanent `will-change:
      // height` in its stylesheet, which promoted a layer for a property
      // that cannot be composited anyway, on every collapsible on screen.
      // `opacity` is the half that genuinely benefits.
      node.style.willChange = "opacity";
      animate(node, {
        height: next ? full : 0,
        opacity: next ? 1 : 0,
        duration: DUR.base,
        ease: EASE,
        onComplete: () => {
          node.style.willChange = "";
          // Guard against a second toggle having landed mid-flight.
          if (last === next) settle(next);
        },
      });
    },
    destroy() {},
  };
}

// ---------------------------------------------------------------------------
// Svelte transitions
//
// For elements the framework adds and removes. These return a `css`
// function rather than driving anime.js directly, because Svelte's own
// scheduler owns the outro and a JS tween racing it is how elements get
// removed mid-fade.
// ---------------------------------------------------------------------------

const CSS_EASE = "cubic-bezier(0.32, 0.72, 0, 1)";

export function riseIn(
  _node: Element,
  { duration = DUR.slow as number, y = 8, delay = 0 } = {},
) {
  return {
    delay,
    duration: motion.ms(duration),
    css: (t: number, u: number) => `opacity:${t}; transform: translateY(${u * y}px);`,
    easing: undefined as undefined,
  };
}

export function popIn(
  _node: Element,
  { duration = DUR.base as number, delay = 0, from = 0.97 } = {},
) {
  return {
    delay,
    duration: motion.ms(duration),
    css: (t: number, u: number) =>
      `opacity:${t}; transform: translateY(${u * 10}px) scale(${from + (1 - from) * t});`,
  };
}

export function slideX(_node: Element, { duration = DUR.base as number, x = 12, delay = 0 } = {}) {
  return {
    delay,
    duration: motion.ms(duration),
    css: (t: number, u: number) => `opacity:${t}; transform: translateX(${u * x}px);`,
  };
}

export { CSS_EASE };

// ---------------------------------------------------------------------------
// MOTION ON SETS OF THINGS
//
// Landing, lifting and shaking: what the tables and the topology move with.
// The workflow canvas these were first written for is gone (DIR-14); the
// two rules it taught stayed. Everything here obeys the three rules at the
// top of this file, plus:
//
//   4. **Compositor-only.** `transform` and `opacity`, and on wires
//      `stroke-dashoffset`. Never `left`/`top`, never `width`/`height` on
//      anything that repeats, never a `filter` or `box-shadow` tween on a
//      set. A forty-node graph cannot afford a layout pass per frame.
//   5. **Bounded fan-out and borrowed layers.** No stagger touches more
//      than ~16 elements; past that the container animates instead. And
//      `will-change` is set when a tween starts and removed when it lands —
//      forty permanently promoted cards is forty permanently spent layers.
// ---------------------------------------------------------------------------

/**
 * Land on *no transform*, not on an identity one.
 *
 * This is `rest()` above, exported, and it is load-bearing for exactly the
 * reason stated there: **any** transform makes an element a stacking
 * context and the containing block for its `position: fixed` descendants.
 * An entrance that finishes on `translateY(0px)` therefore traps every
 * popover that opens inside it — the `{{` picker's `z-index: 40` stops
 * being able to escape the row it lives in, and the rows *after* that one
 * paint straight over it. The symptom is two panels of text interleaved on
 * screen; the cause is one identity transform nobody removed.
 *
 * So every tween in this file that finishes at rest clears the property
 * rather than writing its no-op value.
 */
export function land(targets: ArrayLike<Element> | Element | null | undefined) {
  if (!targets) return;
  const list: Element[] =
    targets instanceof Element ? [targets] : (Array.from(targets) as Element[]);
  for (const el of list) {
    if (!("style" in el)) continue;
    const styled = el as Styled;
    styled.style.transform = "";
    styled.style.opacity = "";
  }
}

/** anime.js animations and timelines are thenable; this makes `await` read. */
type Thenable = { then(cb: () => void): unknown };

/** Resolve when an animation lands. */
export function settle(anim: Thenable): Promise<void> {
  return new Promise((res) => {
    anim.then(() => res());
  });
}

type Styled = Element & ElementCSSInlineStyle;

/**
 * Is there anything to animate?
 *
 * anime.js logs "No target found" for an empty target list, and nearly
 * every set below is legitimately empty some of the time — a card with no
 * chips, a filter that moved nothing, a menu still rendering. A warning
 * that fires in normal operation is a warning nobody reads, so the guard
 * lives here rather than at twenty call sites.
 */
const some = (t: ArrayLike<unknown> | null | undefined): boolean => !!t && t.length > 0;

/**
 * Promote for the length of a tween, then hand the layer back.
 *
 * Returns the release function so it can be dropped into `onComplete`.
 */
export function lift(targets: (Element | null | undefined)[]): () => void {
  const els = targets.filter((e): e is Styled => e != null && "style" in e);
  for (const el of els) el.style.willChange = "transform, opacity";
  return () => {
    for (const el of els) el.style.willChange = "";
  };
}

// --- a failure -------------------------------------------------------------

/** A failure. Damped, bounded, three cycles and done — a card that keeps
 *  shaking is a card that has stopped meaning anything. */
export function shake(node: HTMLElement | null) {
  if (!node || !motion.on) return;
  animate(node, {
    translateX: [0, 9, -7, 5, -3, 0],
    duration: 380,
    ease: "out(2)",
    onComplete: () => utils.set(node, { translateX: 0 }),
  });
}

// --- wires ------------------------------------------------------------------

// --- status -----------------------------------------------------------------

export type Tone = "ok" | "bad" | "live" | "warn";

const WASH: Record<Tone, string> = {
  ok: "var(--status-ok-wash)",
  bad: "var(--status-error-wash)",
  live: "var(--status-running-wash)",
  warn: "var(--status-warn-wash)",
};

/**
 * Status arriving on a card as a wipe rather than a swap.
 *
 * A masked band crosses the ground once and the tone lands underneath it as
 * it passes, so a finishing run reads as a wave down the column instead of
 * forty independent blinks. The band is a real child element — a
 * pseudo-element could only be moved by tweening a custom property, which
 * is a style recalculation every frame.
 *
 * `commit` is called at the moment the band covers the card, which is what
 * makes the wipe *deliver* the state rather than trail it.
 */
export function wash(
  node: HTMLElement | null,
  tone: Tone,
  options: { duration?: number; commit?: () => void } = {},
): Promise<void> {
  const sweep = node?.querySelector<HTMLElement>(".sweep");
  const band = sweep?.querySelector<HTMLElement>("i");
  if (!node || !sweep || !band || !motion.on) {
    options.commit?.();
    return Promise.resolve();
  }
  const ms = options.duration ?? 520;
  sweep.classList.remove("flat");
  sweep.style.setProperty("--sweep-c", WASH[tone]);
  const release = lift([band]);
  utils.set(sweep, { opacity: 1 });
  utils.set(band, { translateX: "-100%" });
  const at = window.setTimeout(() => options.commit?.(), ms * 0.44);
  return settle(animate(band, { translateX: "100%", duration: ms, ease: EASE })).then(() => {
    window.clearTimeout(at);
    options.commit?.();
    animate(sweep, { opacity: 0, duration: DUR.fast, ease: EASE, onComplete: release });
  });
}

// --- text -------------------------------------------------------------------

// --- surfaces ---------------------------------------------------------------

/** A ripple at the point of contact. Appended to `document.body`, removed
 *  when it lands — nothing here outlives its own animation. */
export function ripple(x: number, y: number, tint = "var(--accent)") {
  if (!motion.on) return;
  const r = document.createElement("span");
  r.style.cssText =
    `position:fixed;left:${x}px;top:${y}px;width:14px;height:14px;margin:-7px 0 0 -7px;` +
    `border-radius:50%;border:2px solid ${tint};opacity:0;pointer-events:none;z-index:70;`;
  document.body.appendChild(r);
  animate(r, {
    scale: [0.4, 2.6],
    opacity: [0.9, 0],
    duration: 460,
    ease: "out(3)",
    onComplete: () => r.remove(),
  });
}

/**
 * A menu growing out of the pixel that was clicked.
 *
 * The panel scales from its own corner and the items deal downward. It is
 * 180ms of work that answers "did my click land, and on what?" before a
 * single label has been read.
 */
export function menuIn(menu: HTMLElement | null) {
  if (!menu || !motion.on) return;
  const items = menu.querySelectorAll("button, hr, .head");
  const release = lift([menu, ...items]);
  utils.set(menu, { opacity: 0, scale: 0.9 });
  animate(menu, { opacity: 1, scale: 1, duration: 180, ease: EASE_POP });
  if (!some(items)) return;
  utils.set(items, { opacity: 0, translateX: -8 });
  animate(items, {
    opacity: 1,
    translateX: 0,
    duration: DUR.base,
    ease: EASE,
    delay: stagger(13, { start: 40 }),
    onComplete: () => {
      land(items);
      release();
    },
  });
}

// --- lists ------------------------------------------------------------------

/**
 * FLIP, in three named beats: measure, invert, play.
 *
 * Filtering a list is three events wearing one animation in most tools.
 * This makes them three — rows that survive *slide* to their new position
 * rather than teleporting, so a row somebody was reading stays findable.
 *
 * The measure happens before the DOM is replaced and the invert after, in
 * the same frame, so the browser lays the list out once and the movement is
 * a pure transform on top of a layout that has already settled.
 */
export function flip(
  node: HTMLElement,
  options: { key: unknown; selector?: string; attr?: string } = { key: null },
) {
  const attr = options.attr ?? "data-key";
  const sel = options.selector ?? `[${attr}]`;
  let last = options.key;
  let before = new Map<string, number>();

  const rows = () => Array.from(node.querySelectorAll<HTMLElement>(sel));
  const keyOf = (el: HTMLElement) => el.getAttribute(attr) ?? "";
  const measure = () => {
    before = new Map(rows().map((r) => [keyOf(r), r.getBoundingClientRect().top]));
  };

  const invert = () => {
    const moved: HTMLElement[] = [];
    const entering: HTMLElement[] = [];
    for (const row of rows()) {
      const was = before.get(keyOf(row));
      if (was == null) {
        entering.push(row);
        continue;
      }
      const dy = was - row.getBoundingClientRect().top;
      if (Math.abs(dy) > 0.5) {
        utils.set(row, { translateY: dy });
        moved.push(row);
      }
    }
    // Bounded: a hundred-row catalogue moves as a block, not as a hundred
    // tweens. The cap is on *animated* rows, not on rows shown.
    const cap = 16;
    const m = moved.slice(0, cap);
    const e = entering.slice(0, cap);
    land(moved.slice(cap));
    const release = lift([...m, ...e]);
    if (m.length)
      animate(m, {
        translateY: 0,
        duration: DUR.slow,
        ease: EASE,
        onComplete: () => land(m),
      });
    if (e.length) {
      utils.set(e, { opacity: 0, translateY: 12 });
      animate(e, {
        opacity: 1,
        translateY: 0,
        duration: DUR.slow,
        ease: EASE,
        delay: stagger(26),
        onComplete: () => land(e),
      });
    }
    land(entering.slice(cap));
    window.setTimeout(release, DUR.slow + 120);
  };

  measure();
  return {
    update(next: { key: unknown; selector?: string; attr?: string }) {
      if (next.key === last) return;
      last = next.key;
      if (!motion.on) {
        requestAnimationFrame(measure);
        return;
      }
      // `update` fires before Svelte has patched the children, so the
      // measurement taken on the previous run is the "before" — invert on
      // the next frame, once the new list exists.
      requestAnimationFrame(() => {
        invert();
        measure();
      });
    },
    destroy() {},
  };
}

// ---------------------------------------------------------------------------
// APPLICATION MOTION
//
// The vocabulary the rest of the console moves in — the shell, the tables,
// the topology. Same three rules as the top of the file, and the two above
// (compositor-only, bounded fan-out). Everything here is an action or a
// transition so a view opts in with one attribute and never holds a ref.
// ---------------------------------------------------------------------------

/** A plain fade, for scrims and things that have no direction to come from. */
export function fadeIn(_node: Element, { duration = DUR.base as number, delay = 0 } = {}) {
  return {
    delay,
    duration: motion.ms(duration),
    css: (t: number) => `opacity:${t};`,
  };
}

/**
 * A figure that changed: the new value rolls up into place.
 *
 * `countUp` is for numbers a person watches move. This is for the rest —
 * "3 / 4", "12s ago", "draining" — where the intermediate values would be
 * nonsense but the *fact of the change* still wants saying. One 240ms roll,
 * six pixels, and it is over before it can be watched.
 */
export function roll(node: HTMLElement, key: unknown) {
  let last = key;
  return {
    update(next: unknown) {
      if (next === last) return;
      last = next;
      if (!motion.on) return;
      utils.remove(node);
      animate(node, {
        translateY: [5, 0],
        opacity: [0.2, 1],
        duration: DUR.base,
        ease: EASE,
        onComplete: () => rest(node),
      });
    },
    destroy() {},
  };
}

/**
 * An SVG arc that fills to a percentage.
 *
 * The circle carries `pathLength="100"`, so the dash offset *is* the
 * percentage and nothing here needs to know the radius. Tweened rather
 * than transitioned because a poll can land a new reading mid-flight, and
 * anime.js retargets from wherever the arc is instead of restarting.
 */
export function arc(
  node: SVGElement,
  options: { pct: number; from?: number; delay?: number },
) {
  const clamp = (p: number) => Math.max(0, Math.min(100, p));
  let current = clamp(options.pct);
  if (options.from != null && motion.on) {
    // Mount from `from` (usually 0) so the reading is *delivered* — an arc
    // that fills to 96 says something an arc that appears at 96 does not.
    utils.set(node, { strokeDashoffset: 100 - clamp(options.from) });
    animate(node, {
      strokeDashoffset: 100 - current,
      duration: DUR.slower * 2,
      delay: options.delay ?? 0,
      ease: "out(4)",
    });
  } else {
    utils.set(node, { strokeDashoffset: 100 - current });
  }
  return {
    update(next: { pct: number; from?: number; delay?: number }) {
      const to = clamp(next.pct);
      if (to === current) return;
      current = to;
      if (!motion.on) {
        utils.set(node, { strokeDashoffset: 100 - to });
        return;
      }
      utils.remove(node);
      animate(node, {
        strokeDashoffset: 100 - to,
        duration: DUR.slower * 1.5,
        ease: "out(3)",
      });
    },
    destroy() {
      utils.remove(node);
    },
  };
}

/**
 * A soft aura behind a figure whose presence scales with the load.
 *
 * `level` is 0–1. Below `from` (0.4) there is no aura; from there the
 * opacity climbs to `max` at full load, and above `breathe` (0.7) it
 * breathes — a slow swell whose period shortens from ~2.4s at the
 * threshold to ~0.9s at full. The colour is the element's own fill (a
 * radial gradient the caller picks per tier), so this only ever writes
 * opacity: one compositor-friendly property, no transform, no filter.
 *
 * Quantised into bands so a reading wobbling 71 → 73 → 70 does not restart
 * the loop three times a poll.
 */
export function aura(
  node: SVGElement | HTMLElement,
  options: { level: number; from?: number; breathe?: number; max?: number },
) {
  type Loop = { pause(): unknown };
  let loop: Loop | null = null;
  let band = -1;
  const el = node as HTMLElement;

  const run = (o: typeof options) => {
    const from = o.from ?? 0.4;
    const breathe = o.breathe ?? 0.7;
    const max = o.max ?? 0.85;
    const lvl = Math.max(0, Math.min(1, o.level));
    const next = lvl < from ? -1 : Math.round(((lvl - from) / (1 - from)) * 12);
    if (next === band) return;
    band = next;
    loop?.pause();
    loop = null;
    if (next < 0) {
      if (motion.on) animate(el, { opacity: 0, duration: DUR.slower, ease: EASE });
      else el.style.opacity = "0";
      return;
    }
    const t = next / 12; // 0 … 1 across the visible range
    const base = 0.18 + t * (max - 0.18);
    if (lvl < breathe || !motion.on) {
      if (motion.on) animate(el, { opacity: base, duration: DUR.slower * 1.5, ease: EASE });
      else el.style.opacity = String(base);
      return;
    }
    const heat = (lvl - breathe) / (1 - breathe);
    const period = 2400 - heat * 1500;
    loop = animate(el, {
      opacity: [Math.max(0.08, base - 0.22), Math.min(1, base + 0.12)],
      duration: period,
      ease: "inOut(2)",
      alternate: true,
      loop: true,
    });
  };

  run(options);
  return {
    update(next: typeof options) {
      run(next);
    },
    destroy() {
      loop?.pause();
      loop = null;
    },
  };
}

/**
 * A figure labouring: an outline that expands out of it and fades, again
 * and again, faster the harder the machine is working.
 *
 * Off below `from` (0.8). One ring in flight at a time, so even a fleet
 * pegged at 100% is a set of slow sonar pings rather than a strobe. The
 * ring is scaled about `cx`/`cy` via the transform attribute — SVG-safe,
 * drag-safe, and compositor-only.
 */
export function sonar(
  node: SVGElement,
  options: { level: number; cx: number; cy: number; from?: number },
) {
  type Loop = { pause(): unknown };
  let loop: Loop | null = null;
  let band = -1;
  let cx = options.cx;
  let cy = options.cy;
  const set = (sc: number, o: number) => {
    node.setAttribute("transform", `translate(${cx} ${cy}) scale(${sc.toFixed(4)}) translate(${-cx} ${-cy})`);
    node.style.opacity = String(o);
  };
  set(1, 0);

  const run = (o: typeof options) => {
    const from = o.from ?? 0.8;
    const lvl = Math.max(0, Math.min(1, o.level));
    const next = lvl < from ? -1 : Math.min(4, Math.floor(((lvl - from) / (1 - from)) * 5));
    if (next === band) return;
    band = next;
    loop?.pause();
    loop = null;
    set(1, 0);
    if (next < 0 || !motion.on) return;
    const heat = (next + 1) / 5;
    const st = { sc: 1, o: 0 };
    loop = animate(st, {
      sc: [1, 1.55],
      o: [0.5, 0],
      duration: 1500,
      // The pause between pings is where the rate lives: a beat at full
      // load, three at the threshold.
      loopDelay: 2600 - heat * 2200,
      ease: "out(2)",
      loop: true,
      onUpdate: () => set(st.sc, st.o),
    });
  };

  run(options);
  return {
    update(next: typeof options) {
      cx = next.cx;
      cy = next.cy;
      run(next);
    },
    destroy() {
      loop?.pause();
      loop = null;
      set(1, 0);
    },
  };
}

/**
 * A one-shot ripple out of an SVG figure — selection landing on a peer.
 * The element is expected to sit exactly on the figure; it is scaled about
 * `cx`/`cy` via the transform attribute and left invisible when done.
 */
export function ping(node: SVGElement, options: { key: unknown; cx: number; cy: number }) {
  let last = options.key;
  let cx = options.cx;
  let cy = options.cy;
  const set = (s: number, o: number) => {
    node.setAttribute("transform", `translate(${cx} ${cy}) scale(${s.toFixed(4)}) translate(${-cx} ${-cy})`);
    node.style.opacity = String(o);
  };
  set(1, 0);
  const fire = () => {
    if (!motion.on) return;
    const st = { s: 0.7, o: 0.55 };
    animate(st, {
      s: 2.1,
      o: 0,
      duration: 620,
      ease: "out(3)",
      onUpdate: () => set(st.s, st.o),
      onComplete: () => set(1, 0),
    });
  };
  return {
    update(next: { key: unknown; cx: number; cy: number }) {
      cx = next.cx;
      cy = next.cy;
      if (next.key === last) return;
      last = next.key;
      // Losing the selection is not an event worth a ripple.
      if (next.key != null && next.key !== false) fire();
    },
    destroy() {},
  };
}

/**
 * A meter that fills to its reading.
 *
 * Same argument as `arc`: the poll retargets mid-flight, so the bar is
 * tweened from where it is rather than restarted from where it was. The
 * colour is left to the stylesheet — it is a `color-mix()` of tokens the
 * theme owns, and tweening it in JS would mean resolving every token by
 * hand on every tick.
 */
export function meter(node: HTMLElement, options: { pct: number }) {
  let current = options.pct;
  node.style.width = `${current}%`;
  return {
    update(next: { pct: number }) {
      if (next.pct === current) return;
      current = next.pct;
      if (!motion.on) {
        node.style.width = `${current}%`;
        return;
      }
      utils.remove(node);
      animate(node, { width: `${current}%`, duration: DUR.slower, ease: "out(3)" });
    },
    destroy() {
      utils.remove(node);
    },
  };
}

/**
 * A tab strip whose underline *travels*.
 *
 * Applied to the `.tabrow` container, not to each tab: the action creates
 * its own indicator, watches `aria-selected` flip via a MutationObserver,
 * and slides the bar to the new tab. Six views have a tab row and none of
 * them has to know this exists beyond one attribute — and a static
 * `border-bottom` per tab, which is what they had, cannot say which
 * direction you moved through the set.
 */
export function tabline(node: HTMLElement, options: { selector?: string } = {}) {
  const sel = options.selector ?? '[role="tab"]';
  const bar = document.createElement("i");
  bar.className = "tabline-bar";
  bar.setAttribute("aria-hidden", "true");
  node.classList.add("tabline");
  node.appendChild(bar);
  let first = true;

  const place = () => {
    const active = node.querySelector<HTMLElement>(`${sel}[aria-selected="true"]`);
    if (!active) {
      utils.set(bar, { opacity: 0 });
      return;
    }
    const left = active.offsetLeft;
    const width = active.offsetWidth;
    if (first || !motion.on) {
      utils.set(bar, { translateX: left, width, opacity: 1 });
      first = false;
      return;
    }
    utils.remove(bar);
    animate(bar, {
      translateX: left,
      width,
      opacity: 1,
      duration: DUR.slow,
      ease: EASE_POP,
    });
  };

  const mo = new MutationObserver(() => requestAnimationFrame(place));
  mo.observe(node, {
    subtree: true,
    childList: true,
    attributes: true,
    attributeFilter: ["aria-selected"],
  });
  // Fonts settle a beat after mount and shift the measurement; place twice.
  requestAnimationFrame(place);
  window.setTimeout(place, 220);
  const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(() => place()) : null;
  ro?.observe(node);

  return {
    destroy() {
      mo.disconnect();
      ro?.disconnect();
      bar.remove();
      node.classList.remove("tabline");
    },
  };
}

/** `menuIn`, as an action, so a popover opts in with one attribute. */
export function popMenu(node: HTMLElement) {
  menuIn(node);
  return { destroy() {} };
}

/**
 * Press feedback for every control in the application, in one listener.
 *
 * `pressable` per button would be four hundred actions. One delegated
 * pointerdown on the document finds the nearest control and gives it the
 * same 40ms nudge — and every control acquired at any later point gets it
 * for free. Returns the uninstaller.
 */
const PRESSABLE =
  ".btn, .iconbtn, .mast-btn, .tab, .tab-sm, .rail-item, .switch-btn, .menu-item, .seg > button, .cluster > button";

export function installPressFeedback(scale = 0.965): () => void {
  let held: HTMLElement | null = null;
  const release = () => {
    const el = held;
    held = null;
    if (!el || !motion.on) return;
    utils.remove(el);
    animate(el, {
      scale: 1,
      duration: DUR.base,
      ease: EASE_POP,
      onComplete: () => {
        el.style.transform = "";
      },
    });
  };
  const down = (e: PointerEvent) => {
    if (!motion.on || e.button !== 0) return;
    const el = (e.target as Element | null)?.closest<HTMLElement>(PRESSABLE);
    if (!el || (el as HTMLButtonElement).disabled) return;
    // A control that already carries a transform (a rail marker passing
    // under it, a card mid-entrance) is left alone: stacking a scale on
    // top of a tween in flight is how a button ends up stuck at 96%.
    if (el.style.transform && !el.style.transform.startsWith("scale")) return;
    held = el;
    animate(el, { scale, duration: DUR.instant, ease: "out(2)" });
  };
  document.addEventListener("pointerdown", down, { passive: true });
  document.addEventListener("pointerup", release, { passive: true });
  document.addEventListener("pointercancel", release, { passive: true });
  window.addEventListener("blur", release);
  return () => {
    document.removeEventListener("pointerdown", down);
    document.removeEventListener("pointerup", release);
    document.removeEventListener("pointercancel", release);
    window.removeEventListener("blur", release);
  };
}
