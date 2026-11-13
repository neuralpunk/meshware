<script lang="ts">
  // The signature component (components/code-digit-display.md): six boxes,
  // two tonal groups of three, all six land together — no stagger, this is
  // a security comparison, not a slot machine.
  let { code }: { code: string } = $props();
  const digits = $derived(code.split(""));
</script>

<div class="fade-in flex justify-center gap-2">
  {#each digits as d, i (i)}
    <div
      class="mono grid h-[62px] w-[50px] place-items-center rounded-[7px] text-[28px] font-semibold"
      style={`background: ${i < 3 ? "rgba(94,234,212,.09)" : "rgba(94,234,212,.05)"};
              border: 1px solid rgba(94,234,212,.22); color: var(--brand-mark);
              font-variant-numeric: tabular-nums;`}
    >
      {d}
    </div>
  {/each}
</div>
