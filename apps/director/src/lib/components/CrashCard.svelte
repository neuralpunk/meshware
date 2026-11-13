<script lang="ts">
  // Something went wrong, with a way forward (DIR-11).
  //
  // "Director could not start" used to be a dark card with the raw OS error
  // and nothing to press: no retry, no way to copy what it said, no pointer
  // to the log. The same card now serves that screen and the error
  // boundaries around every view, so a failure anywhere offers the same
  // three things — try again, copy the details, open the logs.
  import { api, errText } from "../api";
  import { crashDetails } from "../crashlog";

  let {
    eyebrow,
    title,
    message,
    stack = null,
    hint = null,
    retryLabel = "Retry",
    onretry,
  }: {
    eyebrow: string;
    title: string;
    message: string;
    stack?: string | null;
    hint?: string | null;
    retryLabel?: string;
    onretry: () => void | Promise<void>;
  } = $props();

  let busy = $state(false);
  let note = $state<string | null>(null);

  async function retry() {
    busy = true;
    note = null;
    try {
      await onretry();
    } finally {
      busy = false;
    }
  }

  async function copy() {
    try {
      await api.copyText(await crashDetails(title, message, stack));
      note = "Details copied";
    } catch (e) {
      note = `Could not copy: ${errText(e)}`;
    }
  }

  async function openLogs() {
    try {
      await api.openLogDir();
      note = null;
    } catch (e) {
      note = errText(e);
    }
  }

  const button =
    "h-9 rounded-md border border-edge-code px-3.5 text-sm font-semibold text-inkcode-2 transition-colors hover:bg-[rgba(160,176,200,0.08)] disabled:opacity-40";
</script>

<div
  class="on-code pop-in w-[520px] max-w-[94vw] rounded-2xl bg-code p-6 text-inkcode"
  style="box-shadow: var(--sh-modal);"
  role="alert"
>
  <div class="mono text-2xs tracking-[0.1em] uppercase text-error-ink">{eyebrow}</div>
  <div class="mt-3 text-md font-semibold">{title}</div>
  {#if hint}
    <p class="mt-2 text-sm leading-relaxed text-inkcode-2">{hint}</p>
  {/if}
  <div class="selectable mono mt-3 max-h-40 overflow-y-auto text-sm leading-relaxed break-words text-inkcode-3">
    {message}
  </div>
  <div class="mt-5 flex flex-wrap items-center gap-2">
    <button class="btn btn-bright h-9 px-4 text-sm" disabled={busy} onclick={retry}>
      {busy ? "Trying…" : retryLabel}
    </button>
    <button class={button} onclick={copy}>Copy details</button>
    <button class={button} onclick={openLogs}>Open logs</button>
    {#if note}
      <span class="text-xs text-inkcode-4" role="status">{note}</span>
    {/if}
  </div>
</div>
