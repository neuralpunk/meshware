<script lang="ts">
  // The trust moment, in three renderings of one code.
  //
  // The digits are the protocol (`mesh_core::pairing`): six of them, and
  // both humans must agree. Everything else here is a way of *reading*
  // them, and each answers a failure of the plain digits:
  //
  //   the word pair — two words carrying the whole code, because reading
  //     "harbor lantern" across a room beats "four two one seven three
  //     three, no, *seven* three";
  //   the QR — because half the machines that pair are headless, and the
  //     answer to "walk round to the rack with a monitor" is a phone.
  //
  // All three come from one renderer in the engine (`pair_code_view`), not
  // from arithmetic in here: the console on the other machine is showing
  // the same three things, and a word pair computed twice in two languages
  // is a word pair that eventually disagrees — which reads to the person
  // comparing them as an attack rather than as a bug.
  import { untrack } from "svelte";
  import { api } from "../api";
  import type { PairCodeView } from "../bindings/PairCodeView";
  import CodeDigits from "./CodeDigits.svelte";

  let {
    code,
    /// Start with the QR open. False on the approver's screen — a laptop
    /// is not the thing anybody photographs — and true where this Director
    /// *is* the joiner and its own screen may be the awkward one.
    qrOpen = false,
  }: { code: string; qrOpen?: boolean } = $props();

  let shown = $state<PairCodeView | null>(null);
  // `qrOpen` is the *initial* state and deliberately not bound: once the
  // person has opened or closed the square, that is their choice, and a
  // parent re-render should not overrule it mid-comparison. `untrack` says
  // so to the compiler rather than leaving it as a warning to wonder about.
  let showQr = $state(untrack(() => qrOpen));

  $effect(() => {
    const want = code;
    if (!want) {
      shown = null;
      return;
    }
    let live = true;
    // A failure here loses the picture, never the pairing: the digits
    // below are rendered from `code` directly and do not wait on this.
    api
      .pairCode(want)
      .then((v) => {
        if (live && v.code === want) shown = v;
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  });
</script>

<div class="fade-in">
  <CodeDigits {code} />

  {#if shown?.words}
    <div class="mt-3 text-center">
      <div class="mono text-2xs tracking-[0.12em] uppercase" style="color: var(--ink-code-4);">
        or say
      </div>
      <div
        class="selectable mt-1 text-[19px] font-semibold tracking-[-.01em]"
        style="color: var(--ink-code-2);"
      >
        {shown.words}
      </div>
    </div>
  {/if}

  {#if shown?.qrPath}
    <div class="mt-3 text-center">
      <button
        class="mono text-2xs tracking-[0.08em] uppercase underline-offset-2 hover:underline"
        style="color: var(--ink-code-4); background: none; border: 0; cursor: pointer;"
        onclick={() => (showQr = !showQr)}
        aria-expanded={showQr}
      >
        {showQr ? "hide the square" : "scan it instead"}
      </button>
    </div>

    {#if showQr}
      <div class="pop-in mt-3 flex justify-center">
        <!--
          White quiet zone and black modules, always — not themed. A QR is
          read by a camera, not a person, and scanners want maximum contrast
          in the polarity they expect. A dark-mode-tinted symbol is a symbol
          that reads on one phone and not the next.
        -->
        <div class="rounded-lg p-2" style="background: #ffffff;">
          <svg
            viewBox="0 0 {shown.qrSize} {shown.qrSize}"
            width="168"
            height="168"
            shape-rendering="crispEdges"
            role="img"
            aria-label="Pairing code {shown.grouped} as a scannable square"
          >
            <rect width={shown.qrSize} height={shown.qrSize} fill="#ffffff" />
            <path d={shown.qrPath} fill="#000000" />
          </svg>
        </div>
      </div>
      <div class="mt-2 text-center text-xs" style="color: var(--ink-code-4);">
        Point a phone at the other machine's square — it should read
        <span class="mono">mw-pair:{shown.code}</span>
      </div>
    {/if}
  {/if}
</div>
