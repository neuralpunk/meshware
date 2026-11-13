<script lang="ts">
  // Detail on demand — one help affordance for the whole application.
  //
  // Director's explanatory copy is the best thing in the product (the
  // 2026-08 review said so in bold), but a paragraph that is always visible
  // is paid for on every glance, and an operator glances two hundred times
  // a day. The fix is not deleting the words; it is moving them one
  // deliberate click away, at the exact place the question arises —
  // Tesler's law relocates complexity, and WCAG 2.2's consistent-help rule
  // says the affordance must look and sit the same everywhere. So: one
  // small "?" that opens one kind of note, in the workflow inspector and
  // the Settings page alike.
  //
  // The panel is position:fixed and measured from the trigger on open,
  // because this component lives inside scrolling inspectors, table cells
  // and overflow-hidden cards — an absolutely-positioned panel would be
  // clipped by the first `overflow: auto` ancestor it met. Scroll, resize,
  // Escape and any outside click all close it; a note you have to fight is
  // worse than a paragraph.

  import Icon from "./Icon.svelte";

  let {
    text = "",
    label = "What is this?",
    children,
  }: {
    /** The note, as a plain string — the classic API. */
    text?: string;
    /** Accessible name for the trigger, e.g. "About the script body". */
    label?: string;
    /** Rich content, when a string is not enough. Wins over `text`. */
    children?: import("svelte").Snippet;
  } = $props();

  let open = $state(false);
  let pos = $state({ top: 0, left: 0, right: null as number | null });
  let trigger: HTMLButtonElement | null = $state(null);
  let panel: HTMLDivElement | null = $state(null);
  const id = `hint-${Math.random().toString(36).slice(2, 9)}`;

  function place() {
    if (!trigger) return;
    const r = trigger.getBoundingClientRect();
    // Below the trigger; hug its left edge unless that would run off the
    // right of the window, in which case hug the right edge instead.
    const past = r.left > window.innerWidth * 0.55;
    pos = {
      top: r.bottom + 6,
      left: past ? 0 : Math.max(8, r.left - 4),
      right: past ? Math.max(8, window.innerWidth - r.right - 4) : null,
    };
  }

  function toggle() {
    open = !open;
    if (open) place();
  }

  function onWindow(e: Event) {
    if (!open) return;
    if (e.type === "keydown") {
      if ((e as KeyboardEvent).key === "Escape") open = false;
      return;
    }
    if (
      e.type === "click" &&
      (trigger?.contains(e.target as Node) || panel?.contains(e.target as Node))
    ) {
      return;
    }
    open = false;
  }
</script>

<svelte:window onkeydown={onWindow} onscroll={onWindow} onresize={onWindow} onclick={onWindow} />

<button
  bind:this={trigger}
  type="button"
  class="hint-btn"
  aria-label={label}
  title={open ? undefined : label}
  aria-expanded={open}
  aria-controls={id}
  onclick={(e) => {
    e.stopPropagation();
    toggle();
  }}
>
  <Icon name="help" size={12} />
</button>

{#if open}
  <div
    bind:this={panel}
    class="hint-panel"
    {id}
    role="note"
    style={`top:${pos.top}px; ${pos.right != null ? `right:${pos.right}px` : `left:${pos.left}px`}`}
  >
    {#if children}
      {@render children()}
    {:else}
      {text}
    {/if}
  </div>
{/if}

<style>
  .hint-btn {
    display: inline-grid;
    place-items: center;
    width: 16px;
    height: 16px;
    flex: none;
    border-radius: 999px;
    color: var(--ink-faint);
    cursor: pointer;
    vertical-align: text-bottom;
    transition:
      color var(--dur-fast) var(--ease),
      background-color var(--dur-fast) var(--ease);
  }
  .hint-btn:hover {
    color: var(--accent-ink);
    background: var(--accent-wash);
  }
  .hint-btn[aria-expanded="true"] {
    color: var(--accent-ink);
    background: var(--accent-wash);
  }

  .hint-panel {
    position: fixed;
    z-index: 90;
    max-width: min(46ch, calc(100vw - 16px));
    padding: 10px 13px;
    border: 1px solid var(--edge-card);
    border-left: 2px solid var(--accent);
    border-radius: 0 var(--radius-lg) var(--radius-lg) 0;
    background: var(--surface-1);
    box-shadow: var(--sh-toast);
    font-size: var(--text-sm);
    font-weight: 400;
    line-height: 1.55;
    text-align: left;
    letter-spacing: normal;
    text-transform: none;
    color: var(--ink-2);
    animation: mw-rise var(--dur-base) var(--ease) backwards;
    user-select: text;
    white-space: normal;
  }
</style>
