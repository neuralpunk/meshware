<script lang="ts">
  // What a node selector actually resolves to, right now.
  //
  // A selector is a small language — `any`, `tag=prod`, `name=hermes`, a
  // glob — and the difference between a grant that covers production and
  // one that covers nothing is a typo. Resolving it live turns "I think
  // this is right" into "this is the list", which is the single cheapest
  // thing this view can do for somebody's confidence.
  import { api } from "../api";
  import type { NodeView } from "../api";

  let {
    selector,
    /** Shown when the selector is empty rather than resolving `any`. */
    placeholder = "no nodes yet",
    /**
     * Also report how many matched nodes would refuse a terminal.
     *
     * A session policy that resolves to four nodes, three of which do not
     * accept terminals, is a policy that will surprise somebody at exactly
     * the wrong moment — so the editor says so while it is still being
     * written rather than at the prompt.
     */
    terminals = false,
  }: { selector: string; placeholder?: string; terminals?: boolean } = $props();

  let matched = $state<NodeView[] | null>(null);
  let error = $state<string | null>(null);
  let resolving = $state(false);

  // Debounced: this fires on every keystroke of a text field, and a
  // round trip per character is a round trip per character.
  $effect(() => {
    const text = selector.trim();
    if (!text) {
      matched = null;
      error = null;
      return;
    }
    let cancelled = false;
    const timer = setTimeout(async () => {
      resolving = true;
      try {
        const rows = await api.targetPreview(text);
        if (!cancelled) {
          matched = rows;
          error = null;
        }
      } catch (e) {
        if (!cancelled) {
          matched = null;
          error = e instanceof Error ? e.message : String(e);
        }
      } finally {
        if (!cancelled) resolving = false;
      }
    }, 220);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  });

  const live = $derived((matched ?? []).filter((n) => !n.revoked && !n.left));
  const names = $derived(live.map((n) => n.name));
  const shown = $derived(names.slice(0, 8));
  const more = $derived(Math.max(0, names.length - shown.length));
  /** Matched nodes that would refuse a terminal, and why in one word. */
  const refusing = $derived(
    live.filter((n) => n.noShell || !n.executing || !n.acceptsTerminals).map((n) => n.name),
  );
</script>

<div class="mt-1.5 min-h-[18px] text-xs leading-relaxed">
  {#if error}
    <span style="color: var(--status-error-ink);">{error}</span>
  {:else if resolving && matched === null}
    <span class="text-ink-6">resolving…</span>
  {:else if matched === null}
    <span class="text-ink-6">{placeholder}</span>
  {:else if names.length === 0}
    <!-- Not an error — a selector that matches nothing today may match
         something tomorrow — but it is worth saying out loud, because a
         grant nobody can use looks exactly like one that works. -->
    <span style="color: var(--status-warn-ink);">
      matches no node right now — this grants nothing until one matches
    </span>
  {:else}
    <span class="text-ink-4">
      matches <span class="num font-semibold text-ink-2">{names.length}</span>
      {names.length === 1 ? "node" : "nodes"}:
    </span>
    {#each shown as name (name)}
      <span class="mono ml-1 rounded-sm bg-surface-2 px-1.5 py-0.5 text-ink-3">{name}</span>
    {/each}
    {#if more}<span class="ml-1 text-ink-5">+{more} more</span>{/if}
    {#if terminals && refusing.length}
      <div class="mt-1" style="color: var(--status-warn-ink);">
        {refusing.length} of them {refusing.length === 1 ? "does" : "do"} not accept terminals
        ({refusing.slice(0, 4).join(", ")}{refusing.length > 4 ? "…" : ""}) — this policy
        permits nothing there until their own operators turn terminals on.
      </div>
    {/if}
  {/if}
</div>
