<script lang="ts">
  // A donut: shares of a whole, or one reading against it, with whatever
  // the caller puts in the middle (2026-09-22).
  //
  // Every ring in Director is this component — the fleet's health, the
  // slot gauge, each machine's load, the capacity gauges, the port
  // ledger, the update rings — so a ring means one thing wherever it is:
  // the whole is the full turn, each share an arc in its own colour,
  // clockwise from twelve.
  //
  // The shapes are exact filled outlines from `lib/arcs.ts`, not dashes
  // on a stroked circle; that module says why, and what the two kinds of
  // ring (gauge and breakdown) look like. This file only animates them:
  // the ring sweeps in clockwise on arrival, and a changed reading glides
  // to its new value rather than jumping. Both are skipped under reduced
  // motion.
  import { untrack, type Snippet } from "svelte";
  import { Tween } from "svelte/motion";
  import { cubicOut } from "svelte/easing";
  import { annulusPath, layoutRing } from "../arcs";
  import { motion } from "../motion";

  export type Arc = { value: number; color: string; label?: string };

  let {
    arcs,
    total,
    size = 112,
    thickness = 11,
    /** Width of the cut between two shares, px. Constant from the hole to
     *  the rim. */
    gap,
    /** Radius of a share's corners, px. */
    corner,
    track = "var(--track)",
    /** Spoken for the whole figure — the ring is drawn, not read. */
    label,
    children,
  }: {
    arcs: Arc[];
    total?: number;
    size?: number;
    thickness?: number;
    gap?: number;
    corner?: number;
    track?: string;
    label?: string;
    children?: Snippet;
  } = $props();

  // Thin enough that the ring still reads as one ring, wide enough that
  // two neighbours never blur into each other at the edge.
  const cut = $derived(gap ?? Math.max(1.5, Math.min(3, thickness * 0.2)));
  const round = $derived(corner ?? Math.max(0.75, Math.min(2.5, thickness * 0.2)));

  const targets = $derived(arcs.map((a) => a.value));

  // The entrance, once: 0 → 1 of the turn.
  const sweep = new Tween(motion.on ? 0 : 1, { duration: 720, easing: cubicOut });
  $effect(() => {
    void sweep.set(1);
  });

  // Each reading, eased from the last one. The first reading is not
  // eased — the sweep is its entrance — and a ring whose shares change in
  // number (a new state appears) starts from where it is, not from zero.
  const shown = new Tween<number[]>([], { duration: 480, easing: cubicOut });
  let first = true;
  $effect(() => {
    const next = targets;
    untrack(() => {
      if (first) {
        first = false;
        void shown.set(next, { duration: 0 });
        return;
      }
      const now = shown.current;
      if (next.length !== now.length) void shown.set(next.map((_, i) => now[i] ?? 0), { duration: 0 });
      void shown.set(next, { duration: motion.ms(480) });
    });
  });

  const layout = $derived(
    layoutRing({
      values: shown.current.length === targets.length ? shown.current : targets,
      total,
      size,
      thickness,
      gap: cut,
      corner: round,
      sweep: sweep.current,
    }),
  );

  const trackPath = $derived(annulusPath(size / 2, size / 2, size / 2, Math.max(0, size / 2 - thickness)));
</script>

<span
  class="relative grid shrink-0 place-items-center"
  style={`width:${size}px;height:${size}px;`}
  role={label ? "img" : undefined}
  aria-label={label}
>
  <svg
    width={size}
    height={size}
    viewBox={`0 0 ${size} ${size}`}
    class="absolute inset-0 overflow-visible"
    shape-rendering="geometricPrecision"
    aria-hidden="true"
  >
    {#if layout.track}
      <path d={trackPath} fill={track} fill-rule="evenodd" />
    {/if}
    {#each layout.shapes as s, i (i)}
      <path d={s.d} fill={s.index === null ? track : arcs[s.index]?.color} fill-rule="evenodd">
        {#if s.index !== null && arcs[s.index]?.label}<title>{arcs[s.index].label}</title>{/if}
      </path>
    {/each}
  </svg>
  {#if children}
    <span class="relative grid place-items-center text-center leading-none">{@render children()}</span>
  {/if}
</span>
