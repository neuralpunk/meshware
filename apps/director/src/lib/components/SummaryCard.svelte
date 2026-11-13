<script lang="ts">
  // A card that is a door (2026-09-22): a glyph, a title, one line of
  // where things stand, a small picture of it — and the whole card opens
  // the full ledger. Capacity's Runtimes, Ports and Volumes are each one of
  // these, so the three read as a set and open the same way.
  //
  // A real <button>, so it is reached by Tab and pressed by Enter or
  // Space like anything else that opens something; its insides are spans
  // for the same reason — a button holds phrasing content only.
  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";

  let {
    icon,
    title,
    sub,
    /** The glyph tile's colour — a hue or status token. */
    tone = "var(--accent)",
    onopen,
    children,
  }: {
    icon: string;
    title: string;
    sub: string;
    tone?: string;
    onopen: () => void;
    children?: Snippet;
  } = $props();
</script>

<button class="panel lift summary" onclick={onopen} aria-label={`${title}: ${sub}. Open the details.`}>
  <span class="panel-head">
    <span class="icon-tile" style={`--tone: ${tone};`}><Icon name={icon} size={17} /></span>
    <span class="min-w-0 flex-1">
      <span class="panel-title block">{title}</span>
      <span class="panel-sub block truncate">{sub}</span>
    </span>
    <span class="more">Details <Icon name="chevron-right" size={13} /></span>
  </span>
  {#if children}<span class="block min-w-0">{@render children()}</span>{/if}
</button>

<style>
  .summary {
    display: flex;
    flex-direction: column;
    width: 100%;
    text-align: left;
  }
  .more {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    flex: none;
    align-self: flex-start;
    font-size: var(--text-xs);
    font-weight: 600;
    color: var(--ink-5);
    transition:
      color var(--dur-fast) var(--ease),
      transform var(--dur-fast) var(--ease);
  }
  .summary:hover .more {
    color: var(--accent-ink);
    transform: translateX(2px);
  }
</style>
