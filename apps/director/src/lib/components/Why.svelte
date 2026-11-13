<script lang="ts">
  // The explanation, attached to the value it explains (L2 P4).
  //
  // `Hint` is the other half of this pair and they are deliberately not the
  // same control. A `?` is a *question a reader might have*: it sits beside
  // a field, it costs a click, and it opens prose. This is an *answer the
  // product already computed* — why that node won, where that range came
  // from, what that key means — and the affordance for it is the value
  // itself, underlined, on hover.
  //
  // The distinction matters because P4's whole complaint is that these
  // explanations existed and were filed behind a command. Putting them one
  // click away behind a glyph would move them from one hiding place to
  // another; putting them under the pointer that is already on the number
  // is the phase.
  //
  // Focusable and keyboard-openable, because a hover-only affordance is one
  // half the people cannot reach. Escape closes it; so does moving away.
  //
  // The panel is position:fixed and measured on open for the same reason
  // `Hint`'s is: this lives inside scrolling inspectors and table cells,
  // and an absolutely-positioned panel would be clipped by the first
  // `overflow: auto` ancestor it met.
  import type { Snippet } from "svelte";

  let {
    text,
    children,
  }: {
    /** The sentence. Composed in `mesh-proto`; never re-worded here. */
    text: string;
    /** The value being explained. */
    children: Snippet;
  } = $props();

  let open = $state(false);
  let pos = $state({ top: 0, left: 0, right: null as number | null });
  let anchor: HTMLSpanElement | null = $state(null);
  const id = `why-${Math.random().toString(36).slice(2, 9)}`;

  function place() {
    if (!anchor) return;
    const r = anchor.getBoundingClientRect();
    const past = r.left > window.innerWidth * 0.55;
    pos = {
      top: r.bottom + 6,
      left: past ? 0 : Math.max(8, r.left - 4),
      right: past ? Math.max(8, window.innerWidth - r.right - 4) : null,
    };
  }

  function show() {
    place();
    open = true;
  }
</script>

<svelte:window
  onscroll={() => (open = false)}
  onresize={() => (open = false)}
  onkeydown={(e) => {
    if (e.key === "Escape") open = false;
  }}
/>

<span
  bind:this={anchor}
  class="why"
  tabindex="0"
  role="button"
  aria-describedby={open ? id : undefined}
  aria-label={text}
  onmouseenter={show}
  onmouseleave={() => (open = false)}
  onfocus={show}
  onblur={() => (open = false)}
  onclick={() => (open ? (open = false) : show())}
  onkeydown={(e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      open ? (open = false) : show();
    }
  }}
>
  {@render children()}
</span>

{#if open}
  <div
    class="why-panel"
    {id}
    role="tooltip"
    style={`top:${pos.top}px; ${pos.right != null ? `right:${pos.right}px` : `left:${pos.left}px`}`}
  >
    {text}
  </div>
{/if}

<style>
  /* A dotted underline, which is the century-old convention for "there is
     more here" and needs no glyph, no colour and no space of its own. */
  .why {
    cursor: help;
    text-decoration: underline;
    text-decoration-style: dotted;
    text-decoration-thickness: 1px;
    text-underline-offset: 3px;
    text-decoration-color: var(--ink-faint);
  }
  .why:hover,
  .why:focus-visible {
    text-decoration-color: currentColor;
  }
  .why:focus-visible {
    outline: 2px solid var(--accent-ring);
    outline-offset: 2px;
    border-radius: 2px;
  }

  /* The same note `Hint` opens, so two explanations in one window read as
     one kind of thing. */
  .why-panel {
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
    text-decoration: none;
    color: var(--ink-2);
    pointer-events: none;
    animation: mw-rise var(--dur-base) var(--ease) backwards;
    white-space: normal;
  }
</style>
