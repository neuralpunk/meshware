<script lang="ts">
  // A light dialog: the editor and the drill-down, lifted off the page.
  //
  // `Modal.svelte` is the *inverse* surface — dark in both themes — and it
  // is reserved for the decision that cannot be undone: delete, clear,
  // admit a stranger. This is the other kind of dialog: a form to fill in,
  // or one row opened for a closer look, on the same light surface as the
  // page behind it. The two must never be confused, which is why they are
  // two components and not one with a `dark` prop.
  //
  // Why a dialog at all, rather than a card on the page: every list screen
  // used to open its editor *above* the table and its detail *inside* it,
  // as an expanded row. A deployment builder is 1,200px of form, and an
  // expanded row pushes every row under it off the bottom of the screen.
  // Both are a thing you look at for a minute and close; neither is the
  // page. The skeleton — header with a glyph tile, scrolling body, footer
  // on the subtle tone — is the one `SshUpgradeSheet` already used, made
  // reusable.
  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";
  import { portal } from "../portal";
  import { popIn, fadeIn, DUR } from "../motion";
  import { holdStill } from "../stillness";
  import { dialog } from "../dialog";

  let {
    open = false,
    title,
    sub = "",
    icon = "",
    width = 680,
    onclose,
    /** Set when the sheet must not be dismissed by the backdrop or Escape
     *  — a publish in flight, say. The close button is disabled too. */
    locked = false,
    children,
    /** Beside the title, after it: a status pill, an id chip. */
    aside,
    footer,
  }: {
    open?: boolean;
    title: string;
    sub?: string;
    icon?: string;
    width?: number;
    onclose: () => void;
    locked?: boolean;
    children: Snippet;
    aside?: Snippet;
    footer?: Snippet;
  } = $props();

  function close() {
    if (!locked) onclose();
  }

  // The page behind stops animating while this is up (M-28), same as the
  // dark modal: a blurred backdrop over a moving topology recomposites
  // every frame.
  $effect(() => (open ? holdStill() : undefined));
</script>

{#if open}
  <div
    use:portal
    class="fixed inset-0 z-50 grid place-items-center p-4"
    style="background: rgba(12,16,24,.55); backdrop-filter: blur(3px);"
    role="presentation"
    transition:fadeIn={{ duration: DUR.base }}
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
  >
    <!-- Escape closes the dialog on *top* (`lib/dialog.ts`): a dark
         confirmation opened from this footer sits above it, and one
         keypress must not take both down. -->
    <div
      class="flex max-h-[92vh] max-w-[96vw] flex-col overflow-hidden rounded-2xl bg-surface"
      in:popIn={{ from: 0.94, duration: DUR.slow }}
      out:popIn={{ from: 0.96, duration: DUR.fast }}
      style={`width:${width}px; box-shadow: var(--sh-modal);`}
      role="dialog"
      aria-modal="true"
      aria-label={title}
      use:dialog={{ onescape: locked ? null : close }}
    >
      <div class="flex flex-none items-start gap-3 px-6 pt-5 pb-4" style="border-bottom: 1px solid var(--edge);">
        {#if icon}
          <span
            class="grid h-9 w-9 shrink-0 place-items-center rounded-lg"
            style="background: var(--hue-wash, var(--accent-wash)); color: var(--hue-ink, var(--accent-ink));"
            aria-hidden="true"
          >
            <Icon name={icon} size={18} />
          </span>
        {/if}
        <div class="min-w-0 flex-1">
          <div class="flex min-w-0 flex-wrap items-center gap-2.5">
            <h2 class="m-0 truncate text-[17px] font-bold tracking-[-.02em] text-ink">{title}</h2>
            {#if aside}{@render aside()}{/if}
          </div>
          {#if sub}
            <p class="mt-0.5 mb-0 text-sm leading-normal text-ink-4" style="text-wrap: pretty;">{sub}</p>
          {/if}
        </div>
        <button class="iconbtn h-7 w-7 shrink-0" title="Close" disabled={locked} onclick={close}>
          <Icon name="close" size={14} />
        </button>
      </div>

      <div class="min-h-0 flex-1 overflow-y-auto px-6 py-5">
        {@render children()}
      </div>

      {#if footer}
        <div
          class="flex flex-none flex-wrap items-center gap-2 px-6 py-4"
          style="border-top: 1px solid var(--edge); background: var(--surface-subtle);"
        >
          {@render footer()}
        </div>
      {/if}
    </div>
  </div>
{/if}
