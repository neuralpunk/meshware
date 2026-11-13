<script lang="ts">
  // Tag chip. `no-execute` is deliberately different: a hole in the mesh,
  // not another colour (dashed border, transparent bg).
  //
  // Every other tag takes a hue from its *key* — `dc=home` and `dc=lab`
  // share one, `role=lab` another, a bare `gpu` its own — so a fleet table
  // reads as families of colour rather than a grey list, and the same tag
  // is the same colour on every screen and every machine (`hueOf` is a
  // hash, not a choice).
  import { hueOf, hueStyle } from "../hues";
  let {
    text,
    /**
     * An admin assigned this tag in the mesh, rather than the node
     * announcing it about itself.
     *
     * Marked, never re-coloured: the whole value of a tag's hue is that
     * `dc=east` is the same colour everywhere, and a chip that changed
     * colour depending on which half it came from would break the one
     * thing the palette is for. The × is the mark — it appears only where
     * removal is possible, so the affordance and the fact are the same
     * thing rather than two things that can disagree.
     */
    assigned = false,
    /** When given, the chip carries an × that calls this. Admin surfaces only. */
    onRemove,
    /** What this tag means here, on hover. */
    title,
  }: {
    text: string;
    assigned?: boolean;
    onRemove?: () => void;
    title?: string;
  } = $props();
  const special = $derived(text === "no-execute");
  const family = $derived(text.split("=")[0] ?? text);
  // Added to the chip's own inset edge rather than replacing it: a style
  // attribute wins outright over the class, so writing `box-shadow` here
  // without the original would silently take the chip's outline off.
  const ASSIGNED_RING =
    "box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--hue, var(--accent)) 18%, transparent), 0 0 0 1.5px var(--edge-strong);";
</script>

{#snippet remove()}
  <!-- Inside the chip, riding its own 5px gap, so it reads as part of the
       tag rather than as punctuation that happened to land beside it. -->
  <button
    class="-mr-[3px] ml-[1px] grid h-[13px] w-[13px] flex-none place-items-center rounded-full leading-none opacity-60 transition-opacity hover:opacity-100"
    style="background: color-mix(in srgb, currentColor 14%, transparent);"
    title={`Remove ${text} from this node`}
    aria-label={`Remove ${text}`}
    onclick={onRemove}>×</button
  >
{/snippet}

{#if special}
  <span
    class="mono inline-flex items-center gap-[5px] rounded-full border-[1.5px] border-dashed px-2 py-0.5 text-2xs font-semibold"
    style="border-color: var(--ink-5); color: var(--ink-3);"
    {title}
    >{text}{#if onRemove}{@render remove()}{/if}</span
  >
{:else}
  <span class="hue-chip" style={`${hueStyle(hueOf(family))}${assigned ? ASSIGNED_RING : ""}`} {title}
    >{text}{#if onRemove}{@render remove()}{/if}</span
  >
{/if}
