<script module lang="ts">
  // A rollout, at any size.
  //
  // The card-per-machine view (`DeployCards`) is right for three machines
  // and wrong for eighty: eighty cards is a wall nobody reads, and the two
  // that failed are somewhere in the middle of it. This is the same event
  // stream, arranged by **what needs a person**:
  //
  //   1. anything that failed, pinned open — the only rows with a decision
  //      attached to them;
  //   2. what is happening right now, which is bounded by the parallelism,
  //      not by the fleet size;
  //   3. everything finished, collapsed to a count, opened on demand.
  //
  // The bar is the whole run in one line, so the answer to "how long" is a
  // glance rather than a scroll. Nothing here is virtualised because
  // nothing here is ever long: the only unbounded list is the finished
  // one, and it starts closed.
  import type { Card } from "./DeployCards.svelte";

  export type Bucket = "failed" | "running" | "done" | "queued";

  export function bucketOf(c: Card): Bucket {
    if (c.stage === "failed") return "failed";
    if (c.stage === "done") return "done";
    if (c.stage === "queued") return "queued";
    return "running";
  }
</script>

<script lang="ts">
  import { STAGE_LABEL } from "./DeployCards.svelte";
  import Icon from "./Icon.svelte";

  let {
    cards,
    target = null,
    finished = null,
    onDecide = null,
  }: {
    cards: Card[];
    /** The version being installed, for the heading. */
    target?: string | null;
    finished?: { done: number; failed: number; cancelled: boolean } | null;
    /** Answer a machine's host-key question (S-M9). A rollout is not a
     *  place for a wall of cards, but "should I trust this machine I have
     *  never met?" still has to be askable — nothing is sent until it is
     *  answered, so the alternative is a row that quietly times out. */
    onDecide?: ((host: string, accept: boolean) => void) | null;
  } = $props();

  const failed = $derived(cards.filter((c) => bucketOf(c) === "failed"));
  const running = $derived(cards.filter((c) => bucketOf(c) === "running"));
  const done = $derived(cards.filter((c) => bucketOf(c) === "done"));
  const queued = $derived(cards.filter((c) => bucketOf(c) === "queued"));
  const asking = $derived(cards.filter((c) => c.keyPrompt && !c.decided && c.stage === "confirming"));
  const settled = $derived(failed.length + done.length);
  const pct = $derived(cards.length ? Math.round((settled / cards.length) * 100) : 0);

  // Finished rows are the ones that are safe to hide, so they are the ones
  // that start hidden. Failures never collapse: a run with a failure in it
  // is a run somebody has to do something about.
  let showDone = $state(false);

  /** "already on 0.6.1" is a different outcome from "installed 0.6.1", and
   *  at fleet size the difference is most of the summary. */
  const skipped = $derived(done.filter((c) => c.done?.note?.startsWith("already on")).length);
</script>

<div class="flex flex-col gap-4">
  <!-- one line for the whole run -->
  <div>
    <div class="flex items-baseline gap-2">
      <span class="text-base font-semibold text-ink">
        {#if finished}
          {finished.cancelled ? "Stopped" : "Finished"}
        {:else}
          Updating {cards.length} node{cards.length === 1 ? "" : "s"}
        {/if}
      </span>
      {#if target}<span class="num text-sm text-ink-4">to {target}</span>{/if}
      <span class="num ml-auto text-sm text-ink-4">{pct}%</span>
    </div>

    <div class="mt-2 flex h-2 overflow-hidden rounded-full" style="background: var(--surface-2);">
      {#if done.length}
        <div style="width: {(done.length / cards.length) * 100}%; background: var(--status-ok);"></div>
      {/if}
      {#if failed.length}
        <div style="width: {(failed.length / cards.length) * 100}%; background: var(--status-error);"></div>
      {/if}
      {#if running.length}
        <div
          class="rollout-live"
          style="width: {(running.length / cards.length) * 100}%; background: var(--accent);"
        ></div>
      {/if}
    </div>

    <div class="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-ink-4">
      <span><span class="num font-semibold text-ink">{done.length}</span> done</span>
      {#if failed.length}
        <span class="text-error"><span class="num font-semibold">{failed.length}</span> failed</span>
      {/if}
      {#if running.length}<span><span class="num">{running.length}</span> in flight</span>{/if}
      {#if queued.length}<span><span class="num">{queued.length}</span> waiting</span>{/if}
      {#if skipped}<span class="text-ink-5">{skipped} already current</span>{/if}
    </div>
  </div>

  <!-- 1. what needs a person -->
  {#if failed.length}
    <div class="rounded-md border" style="border-color: var(--status-error-edge); background: var(--status-error-wash);">
      <div class="flex items-center gap-2 px-4 py-2.5">
        <Icon name="alert" size={14} />
        <span class="text-sm font-semibold" style="color: var(--status-error-ink);">
          {failed.length} need{failed.length === 1 ? "s" : ""} attention
        </span>
      </div>
      <div class="dark-scroll max-h-64 overflow-y-auto px-4 pb-3">
        {#each failed as c (c.host)}
          <div class="border-t py-2 first:border-t-0" style="border-color: var(--status-error-edge);">
            <div class="flex items-baseline gap-2">
              <span class="text-sm font-semibold text-ink">{c.label}</span>
              <span class="text-xs text-ink-5">{STAGE_LABEL[c.failed?.stage ?? ""] ?? c.failed?.stage}</span>
            </div>
            <div class="mt-0.5 text-xs text-ink-3">{c.failed?.message}</div>
            {#if c.failed?.hint}
              <div class="mt-0.5 text-xs" style="color: var(--status-error-ink);">{c.failed.hint}</div>
            {/if}
          </div>
        {/each}
      </div>
    </div>
  {/if}

  <!-- 1b. machines nobody here has met, pinned like a failure: nothing has
       been sent to them and nothing will be until this is answered (S-M9) -->
  {#if onDecide && asking.length}
    <div>
      <div class="label" style="color: var(--status-warn-ink);">Needs you</div>
      <div class="mt-1.5 flex flex-col gap-2">
        {#each asking as c (c.host)}
          <div
            class="rounded-lg px-3 py-2.5"
            style="background: var(--surface-2); box-shadow: 0 0 0 1px var(--status-warn-ring);"
          >
            <div class="flex flex-wrap items-baseline gap-x-2">
              <span class="text-sm font-medium text-ink">{c.label}</span>
              <span class="text-xs text-ink-5">first time Director has seen this machine</span>
            </div>
            <div class="mono mt-1 break-all text-xs text-ink-3">{c.keyPrompt?.fingerprint}</div>
            <div class="mt-2 flex gap-2">
              <button type="button" class="btn btn-secondary h-8 px-3 text-xs" onclick={() => onDecide?.(c.host, false)}>
                Stop
              </button>
              <button type="button" class="btn btn-primary h-8 px-3 text-xs" onclick={() => onDecide?.(c.host, true)}>
                Trust this machine
              </button>
            </div>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  <!-- 2. what is happening now: bounded by parallelism, never by fleet size -->
  {#if running.length}
    <div>
      <div class="label">In flight</div>
      <div class="mt-1.5 flex flex-col gap-1">
        {#each running as c (c.host)}
          <div class="flex items-center gap-2.5 text-sm">
            <span class="rollout-pulse" style="background: var(--accent);"></span>
            <span class="w-40 shrink-0 truncate font-medium text-ink">{c.label}</span>
            <span class="w-36 shrink-0 truncate text-xs text-ink-4">{STAGE_LABEL[c.stage] ?? c.stage}</span>
            <span class="truncate text-xs text-ink-5">{c.detail}</span>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  <!-- 3. everything that worked, out of the way -->
  {#if done.length}
    <div>
      <button
        class="flex w-full items-center gap-2 text-left"
        onclick={() => (showDone = !showDone)}
      >
        <span class="label">{done.length} done</span>
        <span class="text-xs text-ink-5">{showDone ? "hide" : "show"}</span>
      </button>
      {#if showDone}
        <div class="dark-scroll mt-1.5 max-h-64 overflow-y-auto">
          {#each done as c (c.host)}
            <div class="flex items-center gap-2.5 py-1 text-sm">
              <Icon name="check" size={12} />
              <span class="w-40 shrink-0 truncate text-ink-3">{c.label}</span>
              <span class="num text-xs text-ink-5">{c.done?.version ?? ""}</span>
              {#if c.done?.note}<span class="truncate text-xs text-ink-6">{c.done.note}</span>{/if}
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  /* The live segment breathes so a long install does not read as a stall. */
  .rollout-live {
    animation: rollout-breathe 1.6s ease-in-out infinite;
  }
  .rollout-pulse {
    width: 6px;
    height: 6px;
    flex-shrink: 0;
    border-radius: 999px;
    animation: rollout-breathe 1.6s ease-in-out infinite;
  }
  @keyframes rollout-breathe {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.45; }
  }
  @media (prefers-reduced-motion: reduce) {
    .rollout-live, .rollout-pulse { animation: none; }
  }
</style>
