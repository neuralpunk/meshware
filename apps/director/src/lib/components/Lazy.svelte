<script lang="ts">
  // Render a view that is fetched on first use (H-12).
  //
  // Every one of the fifteen views was a static import, so the webview
  // parsed and compiled 1.75 MB of JavaScript — the workflow canvas, the
  // terminal emulator, the whole legacy surface — before the "joining the
  // mesh…" spinner could appear. Someone who only ever opens Mesh and Jobs
  // paid for all of it on every cold start.
  //
  // The module is cached by the bundler after the first load, so switching
  // back to a view is instant; only the first visit waits, and only for a
  // file that is already on the local disk.
  import type { Component } from "svelte";

  let { load }: { load: () => Promise<{ default: Component }> } = $props();

  let Loaded = $state<Component | null>(null);
  let failed = $state<string | null>(null);

  $effect(() => {
    // Re-run when the loader changes — the view switched.
    const loader = load;
    let alive = true;
    Loaded = null;
    failed = null;
    void loader()
      .then((m) => {
        if (alive) Loaded = m.default;
      })
      .catch((e) => {
        if (alive) failed = e instanceof Error ? e.message : String(e);
      });
    return () => {
      alive = false;
    };
  });
</script>

{#if Loaded}
  <Loaded />
{:else if failed}
  <div class="px-1 py-10 text-sm text-ink-5">
    This screen could not be loaded: {failed}
  </div>
{:else}
  <!-- Deliberately blank rather than a spinner. These modules come off the
       local disk in a few milliseconds; a spinner that appears and vanishes
       within one frame reads as a flicker, not as progress. -->
  <div class="min-h-40" aria-busy="true"></div>
{/if}
