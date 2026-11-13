<script lang="ts">
  // The terminal, fetched when one is first opened (DIR-15).
  //
  // `Terminal` brings xterm with it — 289 kB — and Mesh's Fleet panel,
  // which is on screen at startup, imported it statically. So every cold
  // start parsed a terminal emulator that most sessions never open. Each
  // screen that opens a terminal renders this instead, with the same
  // props; the bundler caches the module after the first open.
  import type { Component, ComponentProps } from "svelte";
  import type TerminalView from "./Terminal.svelte";

  let props: ComponentProps<typeof TerminalView> = $props();

  let Loaded = $state<Component<ComponentProps<typeof TerminalView>> | null>(null);
  let failed = $state<string | null>(null);

  $effect(() => {
    let alive = true;
    import("./Terminal.svelte")
      .then((m) => {
        if (alive) Loaded = m.default as unknown as Component<ComponentProps<typeof TerminalView>>;
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
  <Loaded {...props} />
{:else if failed}
  <div class="rounded-xl px-4 py-3 text-sm text-ink-5">The terminal could not be loaded: {failed}</div>
{:else}
  <!-- The terminal's own frame, empty for the few milliseconds the module
       takes off the local disk, so nothing below it jumps. -->
  <div class="h-[380px] rounded-xl bg-code" aria-busy="true"></div>
{/if}
