<script lang="ts">
  // A count as a row of cells: 3 of 4 healthy, drawn as ▮▮▮▯.
  //
  // The list screens print "3/3/4" and leave the reader to divide. For a
  // number under about twelve, cells are read faster than a fraction and
  // the gap is *seen* rather than computed — the same trick the Fleet
  // metric tiles use for executors and slots, at table density.
  //
  // Three colours, in order: `on` cells in the tone, `warn` cells after
  // them in the warning tone, and the remainder in the track. Past
  // `max` cells the row would be too fine to read, so it degrades to a
  // single proportional bar.
  let {
    total,
    on,
    warn = 0,
    tone = "var(--status-ok)",
    max = 16,
    width = 56,
  }: {
    total: number;
    on: number;
    warn?: number;
    tone?: string;
    max?: number;
    width?: number;
  } = $props();

  const n = $derived(Math.max(0, Math.floor(total)));
  const cells = $derived(n <= max ? n : 0);
  const pct = $derived(n > 0 ? (Math.min(on, n) / n) * 100 : 0);
  const warnPct = $derived(n > 0 ? (Math.min(warn, n - on) / n) * 100 : 0);
</script>

{#if cells > 0}
  <span class="inline-flex h-[6px] items-stretch gap-[2px] align-middle" style={`width:${width}px;`} aria-hidden="true">
    {#each { length: cells } as _, i}
      <span
        class="flex-1 rounded-[2px]"
        style={`background:${i < on ? tone : i < on + warn ? "var(--status-warn)" : "var(--track)"};`}
      ></span>
    {/each}
  </span>
{:else if n > 0}
  <span class="meter-track inline-flex h-[6px] overflow-hidden align-middle" style={`width:${width}px;`} aria-hidden="true">
    <span style={`width:${pct}%; background:${tone};`}></span>
    <span style={`width:${warnPct}%; background:var(--status-warn);`}></span>
  </span>
{/if}
