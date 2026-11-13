<script lang="ts">
  // A collapsible section with its state on the outside.
  //
  // The header answers the question the section answers, without opening
  // it: "Where it runs · Any node", "If it fails · 3 attempts · run stops".
  // That is the whole design. A fold that only says "Where it runs" makes
  // somebody open all four to find the one they changed, which is worse
  // than the flat panel it replaced.
  //
  // Open/closed is remembered per `remember` key and read *synchronously*
  // at construction: seeding from a prop and correcting in an effect makes
  // every remembered-closed fold flash open on mount.

  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";
  import Hint from "./Hint.svelte";
  import { collapsible } from "../motion";

  let {
    title,
    summary = "",
    help = "",
    /** Draw attention: this section holds something other than the default. */
    marked = false,
    /** Storage key. Omit for a fold whose state should not be sticky. */
    remember = "",
    /** What it does the very first time, before anybody has an opinion. */
    startOpen = false,
    children,
  }: {
    title: string;
    summary?: string;
    help?: string;
    marked?: boolean;
    remember?: string;
    startOpen?: boolean;
    children: Snippet;
  } = $props();

  /** `remember` is a literal at every call site, so reading it inside these
   *  keeps Svelte's "initial value" check happy without a closure dance. */
  function key(): string {
    return remember ? `fold:${remember}` : "";
  }

  function restore(): boolean {
    const k = key();
    if (!k) return startOpen;
    try {
      const saved = localStorage.getItem(k);
      return saved == null ? startOpen : saved === "1";
    } catch {
      return startOpen;
    }
  }

  let open = $state(restore());

  function toggle() {
    open = !open;
    const k = key();
    if (!k) return;
    try {
      localStorage.setItem(k, open ? "1" : "0");
    } catch {
      // A disabled store costs stickiness, not the feature.
    }
  }
</script>

<section class="fold" class:open>
  <h4>
    <button type="button" aria-expanded={open} onclick={toggle}>
      <span class="chev" aria-hidden="true"><Icon name="chevron-right" size={13} /></span>
      <span class="title">{title}</span>
      {#if summary}<span class="summary" class:marked>{summary}</span>{/if}
    </button>
    {#if help}<Hint text={help} label={`About ${title.toLowerCase()}`} />{/if}
  </h4>

  <div class="body" use:collapsible={open}>
    <div class="inner">
      {@render children()}
    </div>
  </div>
</section>

<style>
  .fold {
    border-top: 1px solid var(--edge);
    padding-top: 9px;
  }
  h4 {
    display: flex;
    align-items: center;
    gap: 4px;
    margin: 0;
  }
  h4 button {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
    padding: 2px 0;
    border: none;
    background: none;
    font: inherit;
    text-align: left;
    cursor: pointer;
    color: var(--ink-2);
  }
  .chev {
    display: grid;
    place-items: center;
    flex: none;
    color: var(--ink-faint);
    transition: transform var(--dur-fast) var(--ease);
  }
  .fold.open .chev {
    transform: rotate(90deg);
  }
  .title {
    font-size: var(--text-base);
    font-weight: 600;
    white-space: nowrap;
  }
  /* The answer, in the header. Truncated rather than wrapped: a summary
     that takes two lines is not a summary. */
  .summary {
    flex: 1;
    min-width: 0;
    text-align: right;
    font-size: var(--text-xs);
    color: var(--ink-5);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .summary.marked {
    color: var(--accent-ink);
    font-weight: 600;
  }
  h4 button:hover .title {
    color: var(--ink-1);
  }

  /* The `.body` ruleset is gone with its only declaration (L-22).
     It carried a permanent `will-change: height` — a property that cannot
     be composited, so the promotion bought a layer per collapsible on
     screen and nothing else. `motion.ts`'s `collapsible` now sets the hint
     for the duration of its tween and clears it after, which is the rule
     `lift()` already followed. `collapsible` still writes height, opacity,
     overflow and visibility on this element directly. */
  .inner {
    display: flex;
    flex-direction: column;
    gap: 9px;
    padding-top: 9px;
  }
</style>
