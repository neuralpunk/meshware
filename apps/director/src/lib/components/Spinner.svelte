<script lang="ts">
  // Work in progress, as a turning arc (2026-09-22).
  //
  // Every spinner in Director was a `border-radius: 999px` box with one
  // coloured border side. A border side on a circle is not an arc: it
  // tapers to nothing at both corners, its ends are cut on the diagonal,
  // and at 12px it reads as a smudge. This is the same geometry as `Ring`
  // — a round-capped quarter of an exact annulus on its track — turning.
  import { annulusPath, sectorPath } from "../arcs";

  let {
    size = 16,
    thickness = 2,
    color = "var(--accent-ink)",
    track = "color-mix(in srgb, currentColor 16%, transparent)",
    /** Said aloud; decorative when absent, because the words beside a
     *  spinner nearly always say what it is waiting for. */
    label,
  }: { size?: number; thickness?: number; color?: string; track?: string; label?: string } = $props();

  const c = $derived(size / 2);
  const ri = $derived(Math.max(0, c - thickness));
  const trackPath = $derived(annulusPath(c, c, c, ri));
  const arcPath = $derived(sectorPath(c, c, c, ri, 0, Math.PI * 0.55, 0, thickness / 2));
</script>

<svg
  class="spin inline-block shrink-0"
  width={size}
  height={size}
  viewBox={`0 0 ${size} ${size}`}
  shape-rendering="geometricPrecision"
  style={`color: ${color};`}
  role={label ? "img" : undefined}
  aria-label={label}
  aria-hidden={label ? undefined : "true"}
>
  <path d={trackPath} fill={track} fill-rule="evenodd" />
  <path d={arcPath} fill="currentColor" />
</svg>
