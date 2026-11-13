<script lang="ts">
  // How much of this peer's op log the node we are talking to had, the last
  // time it asked us for one.
  //
  // Three honesty rules shape this cell, and they are why it is not just a
  // percentage bar:
  //
  //  1. **It is a statement about one conversation.** A gossip mesh has no
  //     vantage point from which "the mesh is 97% synced" would mean
  //     anything. The number is always "from here, as of then", and the
  //     tooltip says so in those words.
  //  2. **Caught up is the normal case and should be quiet.** A fleet where
  //     every row shouts 100% is a fleet where the one row at 83% is hard to
  //     find. So a synced peer gets a word, not a bar.
  //  3. **Behind is not broken.** Every mesh is behind between gossip
  //     rounds. Lag only becomes a warning when the *reading itself* has
  //     gone stale — a peer that is behind and has stopped asking is a
  //     different thing from a peer that is mid-catch-up.

  import type { SyncProgressView } from "../bindings/SyncProgressView";

  let {
    sync,
    name,
    compact = false,
  }: { sync: SyncProgressView | null; name: string; compact?: boolean } = $props();

  /** A gossip round is seconds. Past this the reading describes a
   *  conversation that is no longer happening. */
  const STALE_AFTER_SECS = 30;

  const ageSecs = $derived(sync ? sync.ageMicros / 1e6 : 0);
  const cold = $derived(ageSecs > STALE_AFTER_SECS);
  const behind = $derived(sync?.behind ?? 0);

  // Fill colors for the bar; the text next to it uses the -ink variants,
  // which are the same hues pushed to AA contrast for small text.
  const tone = $derived.by(() => {
    if (!sync) return "var(--ink-6)";
    if (behind === 0) return "var(--status-ok)";
    return cold ? "var(--status-warn)" : "var(--accent)";
  });
  const toneInk = $derived.by(() => {
    if (!sync) return "var(--ink-6)";
    if (behind === 0) return "var(--status-ok-ink)";
    return cold ? "var(--status-warn-ink)" : "var(--accent-ink)";
  });

  function ago(secs: number): string {
    if (secs < 1) return "just now";
    if (secs < 60) return `${Math.round(secs)}s ago`;
    if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
    return `${Math.round(secs / 3600)}h ago`;
  }

  const title = $derived.by(() => {
    if (!sync) return `${name} has not synced with this peer since it started.`;
    const when = `Last asked ${ago(ageSecs)}.`;
    if (behind === 0)
      return `${name} had every change this machine knows of — all ${sync.total.toLocaleString()}. ${when}`;
    const ops = `${behind.toLocaleString()} change${behind === 1 ? "" : "s"}`;
    const authors = `${sync.authors} machine${sync.authors === 1 ? "" : "s"}`;
    return cold
      ? `${name} was ${ops} behind, made on ${authors} — and has not asked since. ${when} Machines normally check in every few seconds: check it is reachable.`
      : `${name} is catching up: ${ops} behind of ${sync.total.toLocaleString()}, made on ${authors}. ${when}`;
  });
</script>

{#if !sync}
  <span class="text-xs text-ink-6" {title}>—</span>
{:else if behind === 0}
  <!-- Caught up says so in one word. No bar: a column of full bars is a
       column nobody reads, and it would bury the row that is not full. -->
  <span class="text-xs whitespace-nowrap" style={`color:${cold ? "var(--ink-5)" : "var(--status-ok-ink)"};`} {title}>
    {cold ? "in sync ·" : "in sync"}{#if cold}<span class="text-ink-6"> {ago(ageSecs)}</span>{/if}
  </span>
{:else}
  <span class="flex items-center gap-2 whitespace-nowrap" {title}>
    <span class="h-1.5 w-14 flex-none overflow-hidden rounded-full bg-surface-2">
      <span class="block h-full rounded-full transition-[width] duration-500" style={`width:${Math.max(2, sync.pct)}%; background:${tone};`}></span>
    </span>
    <span class="num text-xs" style={`color:${toneInk};`}>{sync.pct}%</span>
    {#if !compact}
      <span class="num text-2xs text-ink-5">−{behind.toLocaleString()}</span>
    {/if}
  </span>
{/if}
