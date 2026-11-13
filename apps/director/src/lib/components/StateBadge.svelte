<script lang="ts">
  // Job state badge (components/chips-badges-liveness.md). Every badge
  // shows its word — colour never carries the state alone.
  let { state }: { state: string } = $props();

  const styles: Record<string, { bg: string; ink: string; dot: string; breathe?: boolean }> = {
    queued: { bg: "var(--surface-2)", ink: "var(--ink-4)", dot: "var(--ink-5)" },
    running: { bg: "var(--status-running-wash)", ink: "var(--status-running-ink)", dot: "var(--status-running)", breathe: true },
    succeeded: { bg: "var(--status-ok-wash)", ink: "var(--accent-ink)", dot: "var(--status-ok)" },
    failed: { bg: "var(--status-error-wash)", ink: "var(--status-error-ink)", dot: "var(--status-error)" },
    cancelled: { bg: "var(--surface-2)", ink: "var(--ink-5)", dot: "var(--liveness-dead)" },
    "timed-out": { bg: "var(--status-warn-wash)", ink: "var(--status-warn-ink)", dot: "var(--status-warn)" },
  };
  const s = $derived(styles[state] ?? styles.queued);
</script>

<span
  class="mono inline-flex items-center gap-[5px] rounded-full px-2 py-0.5 text-2xs font-semibold whitespace-nowrap"
  style={`background:${s.bg};color:${s.ink};`}
>
  <span
    class="inline-block h-[5px] w-[5px] rounded-full"
    class:mw-ambient={s.breathe}
    style={`background:${s.dot};${s.breathe ? "animation: mw-breathe var(--motion-breathe);" : ""}`}
  ></span>{state}</span
>
