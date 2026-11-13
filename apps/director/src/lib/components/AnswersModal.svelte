<script lang="ts">
  // The answers, in a window of their own.
  //
  // A table with one row per node was drawn inline on the Run page and
  // inside the Jobs › Answers list, and at four rows that was the right
  // call. At four hundred it was the wrong one: the table *became* the
  // page, the fan-out grid under it was a scroll away nobody took, and on
  // Jobs an expanded set pushed every other set off the bottom. An answer
  // set is a document you read, so it opens like one — a light dialog, as
  // wide as the window sensibly allows, the header pinned and the rows
  // scrolling inside it — and Esc puts the page back exactly as it was.
  //
  // Light, not the inverse trust surface `Modal.svelte` draws: nothing here
  // is a decision about who joins the mesh. It is the Facts panel's frame,
  // for the same reason the Facts panel has it.
  //
  // **The height is decided by the set, once.** A four-row answer is a
  // small dialog; anything past `TALL_FROM` takes the full height it is
  // allowed and keeps it, because the table inside windows its rows against
  // a viewport and a viewport that resizes as you filter is a scrollbar
  // that jumps under the pointer.
  import { dialog } from "../dialog";
  import type { ResultRowView } from "../api";
  import ResultTable from "./ResultTable.svelte";
  import { portal } from "../portal";
  import { popIn, fadeIn, DUR } from "../motion";
  import { holdStill } from "../stillness";

  let {
    open = false,
    rows,
    expected = null,
    caption = "",
    source = "",
    onclose,
  }: {
    open?: boolean;
    rows: ResultRowView[];
    expected?: { expression: string; passed: number; failed: string[] } | null;
    caption?: string;
    /** Names the file Save writes — a fan-out id, a job id. */
    source?: string;
    onclose?: () => void;
  } = $props();

  /** Past this the dialog takes the whole height it is allowed. */
  const TALL_FROM = 14;

  // Nothing behind the scrim animates while this is up: a `backdrop-filter`
  // over a topology that keeps moving re-composites the blurred region
  // every frame (M-28).
  $effect(() => (open ? holdStill() : undefined));

  // Focus goes *into* the table — `ResultTable` takes it, because it is
  // the thing that listens for ↑↓, ↵ and / — and comes back here on close,
  // so the button that opened this is still where the person left it.
  $effect(() => {
    if (!open) return;
    const returnTo = document.activeElement as HTMLElement | null;
    return () => returnTo?.focus?.({ preventScroll: true });
  });
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-6"
    style="background: rgba(12,16,24,.62); backdrop-filter: blur(3px);"
    role="presentation"
    transition:fadeIn={{ duration: DUR.base }}
    onclick={(e) => {
      if (e.target === e.currentTarget) onclose?.();
    }}
  >
    <div
      class="flex w-full flex-col overflow-hidden rounded-2xl bg-surface"
      class:h-full={rows.length > TALL_FROM}
      style="max-width: min(1680px, 100%); max-height: min(1080px, 100%); box-shadow: var(--sh-modal), 0 0 0 1px var(--edge-card);"
      role="dialog"
      aria-modal="true"
      aria-label="Answers"
      use:dialog={{ onescape: onclose ?? null }}
      in:popIn={{ from: 0.96, duration: DUR.slow }}
      out:popIn={{ from: 0.98, duration: DUR.fast }}
    >
      <ResultTable {rows} {expected} {caption} {source} fill {onclose} />
    </div>
  </div>
{/if}
