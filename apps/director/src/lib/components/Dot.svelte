<script lang="ts">
  // Liveness / status dot (components/chips-badges-liveness.md): 7px;
  // alive + running breathe, everything else is static. `live` adds the
  // pulsing halo variant.
  //
  // **Not colour alone** (DIR-12). A dot was the only thing saying whether a
  // machine is up, and a colour is nothing to a screen reader and little to
  // somebody who cannot tell the red from the green. It is named now — the
  // same words the Fleet filters use — and says so on hover. `decorative`
  // is for a dot beside text that already says the same, so it is not read
  // twice.
  let {
    kind = "alive",
    live = false,
    size = 7,
    label = null,
    decorative = false,
  }: {
    kind?: string;
    live?: boolean;
    size?: number;
    /** Overrides the word for `kind`. */
    label?: string | null;
    decorative?: boolean;
  } = $props();

  const words: Record<string, string> = {
    alive: "up",
    suspect: "not replying",
    dead: "down",
    running: "running",
    queued: "waiting",
    succeeded: "succeeded",
    failed: "failed",
    cancelled: "cancelled",
    "timed-out": "timed out",
    quarantined: "quarantined",
  };
  const name = $derived(label ?? words[kind] ?? kind);

  const colors: Record<string, string> = {
    alive: "var(--liveness-alive)",
    suspect: "var(--liveness-suspect)",
    dead: "var(--liveness-dead)",
    running: "var(--status-running)",
    queued: "var(--ink-5)",
    succeeded: "var(--status-ok)",
    failed: "var(--status-error)",
    cancelled: "var(--liveness-dead)",
    "timed-out": "var(--status-warn)",
    // A machine isolated as possibly compromised (E2.4, UX-4): red, and
    // still — it may be answering, but nobody should be sending it work.
    quarantined: "var(--status-error)",
  };
  const color = $derived(colors[kind] ?? "var(--ink-5)");
  const animates = $derived(kind === "alive" || kind === "running");
</script>

<span
  class="relative inline-block shrink-0 rounded-full"
  role={decorative ? undefined : "img"}
  aria-label={decorative ? undefined : name}
  aria-hidden={decorative ? "true" : undefined}
  title={decorative ? undefined : name}
  class:mw-ambient={animates && !live}
  style={`width:${size}px;height:${size}px;background:${color};${animates && !live ? "animation: mw-breathe var(--motion-breathe);" : ""}`}
>
  {#if live}
    <span
      class="mw-ambient absolute inset-0 rounded-full"
      style={`background:${color};animation: mw-pulse 2.4s ease-out infinite;`}
    ></span>
  {/if}
</span>
