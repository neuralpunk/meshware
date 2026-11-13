<script lang="ts">
  // The app icon itself, not a UI glyph — the same artwork the bundle ships
  // as .icns/.ico/.png, taken from `meshware-icons/app/director/` (direction
  // 7b, "Blade & Tower": three stacked blades, one amber LED each).
  //
  // The teal and the face colours are hard-coded rather than tokenised on
  // purpose. This is the mark people pick out of a dock, a taskbar and an
  // alt-tab strip; a logo that restyles itself when the app flips to dark
  // stops being the thing they recognised. Everything *else* in the sidebar
  // still follows the theme.
  //
  // **Two cuts, not one scaled cut.** The icon set ships a separate "small"
  // drawing for ≤ 32 px — bigger mark in the tile, thicker seams, a larger
  // LED — because the 1024 artwork's 2.5-unit seams and 3-unit LED collapse
  // into mud at rail size. Downscaling the big one is exactly the thing the
  // set exists to avoid, so the switch is here rather than in each caller.
  import large from "../icons/brand-icon.svg?raw";
  import small from "../icons/brand-icon-small.svg?raw";

  let { size = 30 }: { size?: number } = $props();

  const mark = $derived(size <= 32 ? small : large);
</script>

<span
  class="brand-mark block flex-none"
  style={`width:${size}px; height:${size}px; border-radius:${(size * 0.225).toFixed(2)}px;`}
  aria-hidden="true"
>
  {@html mark}
</span>

<style>
  .brand-mark {
    /* Same lift the old tile had, re-anchored to the brand teal so it reads
       on both the light (#e9ecf2) and dark (#0a0e14) sidebar ground. */
    box-shadow: 0 4px 10px -4px rgba(18, 133, 122, 0.7);
    /* The artwork draws its own 22.5% tile, so the radius above is only
       there to clip the shadow to the same silhouette. */
    overflow: hidden;
  }

  .brand-mark :global(svg) {
    display: block;
    width: 100%;
    height: 100%;
  }
</style>
