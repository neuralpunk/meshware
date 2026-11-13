<script lang="ts">
  // Inverse modal: dark in both themes, kicker row (mono uppercase
  // accent-bright left, peer identity right). Esc is the non-destructive
  // dismissal.
  //
  // 436px and a 10px radius come from the console handoff — the same trim
  // every other surface took. It is still the widest thing in the product
  // that is *not* a page, which is the point: a confirmation that decides
  // who joins the mesh should not look like a toast.
  import type { Snippet } from "svelte";
  import { portal } from "../portal";
  import { popIn, fadeIn, DUR } from "../motion";
  import { holdStill } from "../stillness";
  import { dialog } from "../dialog";
  let {
    open = false,
    kicker = "",
    peer = "",
    width = 436,
    onclose,
    children,
  }: {
    open?: boolean;
    kicker?: string;
    peer?: string;
    width?: number;
    onclose?: () => void;
    children?: Snippet;
  } = $props();

  // While this is up, the app behind it stops animating (M-28). A
  // `backdrop-filter` over a topology that keeps moving has to re-composite
  // the whole blurred region every frame; over a still one it composites
  // once.
  $effect(() => (open ? holdStill() : undefined));
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center"
    style="background: rgba(12,16,24,.62); backdrop-filter: blur(3px);"
    role="presentation"
    transition:fadeIn={{ duration: DUR.base }}
    onclick={(e) => {
      if (e.target === e.currentTarget) onclose?.();
    }}
  >
    <div
      class="on-code max-w-[94vw] overflow-hidden rounded-2xl"
      in:popIn={{ from: 0.94, duration: DUR.slow }}
      out:popIn={{ from: 0.96, duration: DUR.fast }}
      style={`width:${width}px; background: var(--surface-code); box-shadow: var(--sh-modal); color: #e7edf4;`}
      role="dialog"
      aria-modal="true"
      use:dialog={{ onescape: onclose ?? null, label: kicker || null }}
    >
      {#if kicker || peer}
        <div class="flex items-center gap-2.5 px-[22px] pt-[18px]">
          <span
            class="mono text-2xs tracking-[0.1em] uppercase"
            style="color: var(--accent-bright);">{kicker}</span
          >
          {#if peer}
            <span class="mono ml-auto text-[11px] text-inkcode-4">{peer}</span>
          {/if}
        </div>
      {/if}
      {@render children?.()}
    </div>
  </div>
{/if}
