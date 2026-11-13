<script lang="ts" module>
  // Director's icons (2026-09-22).
  //
  // One family, drawn to one set of rules: Lucide (ISC — `icons/LICENSE-lucide.txt`,
  // and `icons/README.md` for which Lucide glyph each name is). A 24-unit
  // grid, round caps and joins, outlines only, 2px of padding. Every file
  // in `lib/icons/` is registered here by its name, so an icon is added by
  // dropping in a file drawn to those rules and nothing else.
  //
  // The set this replaced was drawn on a 20-unit grid with 1.6-unit
  // strokes at arbitrary decimals (`cx="4.6" r="2.15"`) and shown at 13,
  // 14 or 15px: strokes of about a pixel, falling between pixels, which is
  // the fuzz people saw. Two rules below fix what the grid alone cannot.
  const files = import.meta.glob<string>("../icons/*.svg", {
    query: "?raw",
    import: "default",
    eager: true,
  });
  const icons: Record<string, string> = Object.fromEntries(
    Object.entries(files)
      .filter(([path]) => !path.includes("/brand-"))
      .map(([path, svg]) => [path.slice(path.lastIndexOf("/") + 1, -4), svg]),
  );

  /**
   * **Even sizes only.** A glyph's centre is its 12th unit; at an odd
   * size that falls on half a pixel and every symmetric shape — a circle,
   * a cross, a chevron — is smeared across two. An odd size is drawn one
   * pixel larger, which no layout here notices and every icon does.
   */
  function snap(size: number): number {
    const s = Math.round(size);
    return s % 2 === 0 ? s : s + 1;
  }

  /**
   * **One weight on screen, whatever the size.** Lucide's 2-unit stroke
   * is 2px at 24px and 1.2px at 14px, so the small icons — most of them
   * — were the faint ones. The stroke is set in screen pixels instead:
   * heavier where the glyph is small, easing to Lucide's own weight at
   * the size it was drawn for.
   */
  function strokeFor(size: number): number {
    const px = size <= 12 ? 1.35 : size <= 14 ? 1.5 : size <= 16 ? 1.6 : size <= 20 ? 1.75 : 2;
    return (px * 24) / size;
  }
</script>

<script lang="ts">
  let { name, size = 16 }: { name: string; size?: number } = $props();

  const px = $derived(snap(size));
</script>

<span
  class="icon inline-grid shrink-0 place-items-center"
  style={`width:${px}px;height:${px}px;--icon-stroke:${strokeFor(px).toFixed(3)};`}
  aria-hidden="true">{@html icons[name] ?? ""}</span
>

<style>
  .icon > :global(svg) {
    width: 100%;
    height: 100%;
    stroke-width: var(--icon-stroke);
    overflow: visible;
  }
</style>
