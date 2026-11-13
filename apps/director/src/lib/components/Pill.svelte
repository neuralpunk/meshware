<script lang="ts">
  // A status word in its tone (components/chips-badges-liveness.md).
  //
  // `StateBadge` knows the six job states and nothing else; the N-track
  // screens each grew their own `rounded px-2 py-0.5` span with a
  // `statusWash()`/`statusInk()` pair beside it, so "active" was a square
  // chip on Deployments and a different square chip on Schedules, and
  // neither matched the round badge one screen over on Activity. One
  // component, one shape, and the caller says only which *tone* the word
  // is in — the word itself is always shown, because colour never carries
  // the state alone.
  let {
    tone = "neutral",
    /** A pulsing dot: this is happening right now. */
    live = false,
    title,
    children,
  }: {
    tone?: "ok" | "warn" | "error" | "running" | "info" | "neutral" | "muted";
    live?: boolean;
    title?: string;
    children: import("svelte").Snippet;
  } = $props();

  const TONES: Record<string, { bg: string; ink: string; dot: string }> = {
    ok: { bg: "var(--status-ok-wash)", ink: "var(--status-ok-ink)", dot: "var(--status-ok)" },
    warn: { bg: "var(--status-warn-wash)", ink: "var(--status-warn-ink)", dot: "var(--status-warn)" },
    error: { bg: "var(--status-error-wash)", ink: "var(--status-error-ink)", dot: "var(--status-error)" },
    running: { bg: "var(--status-running-wash)", ink: "var(--status-running-ink)", dot: "var(--status-running)" },
    info: { bg: "var(--hue-wash, var(--accent-wash))", ink: "var(--hue-ink, var(--accent-ink))", dot: "var(--hue, var(--accent))" },
    neutral: { bg: "var(--surface-2)", ink: "var(--ink-3)", dot: "var(--ink-5)" },
    muted: { bg: "var(--surface-2)", ink: "var(--ink-5)", dot: "var(--liveness-dead)" },
  };
  const t = $derived(TONES[tone] ?? TONES.neutral);
</script>

<span
  class="mono inline-flex items-center gap-[5px] rounded-full px-2 py-0.5 text-2xs font-semibold whitespace-nowrap"
  style={`background:${t.bg};color:${t.ink};`}
  {title}
>
  <span
    class="inline-block h-[5px] w-[5px] rounded-full"
    class:mw-ambient={live}
    style={`background:${t.dot};${live ? "animation: mw-breathe var(--motion-breathe);" : ""}`}
  ></span>{@render children()}</span
>
