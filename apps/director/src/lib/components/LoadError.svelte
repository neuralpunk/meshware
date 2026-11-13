<script lang="ts">
  // A load that failed, said as a failure (UX-2).
  //
  // The screens used to have two states for a list: rows, or an empty state
  // inviting you to make the first one. A request the engine refused fell
  // into the second, so a person without a grant was told the data did not
  // exist and handed the button to create it. This is the third state, and
  // it replaces the empty state rather than sitting on top of it: a call
  // to action is exactly what must not be offered while we do not know
  // what is there.
  //
  // The reason is the engine's own words, selectable, because it is what
  // somebody pastes to an admin. A refusal gets one plain sentence under
  // it saying what to do about it; any other failure gets Retry alone.
  import { app } from "../state.svelte";
  import Icon from "./Icon.svelte";

  let {
    what,
    error,
    retry,
    class: klass = "",
  }: {
    /** What could not be read, as a noun phrase: "the secrets". */
    what: string;
    error: string;
    /** Defaults to asking the engine for every shared table again. */
    retry?: () => Promise<unknown> | void;
    class?: string;
  } = $props();

  let busy = $state(false);

  // The engine's refusals all come through `IpcResponse::Denied`, which
  // Director prints as "permission denied: …" (`engine.rs`); the RBAC text
  // inside it is "requires one of: …".
  const denied = $derived(/permission denied|requires one of/i.test(error));

  async function again() {
    busy = true;
    try {
      await (retry ?? (() => app.retryLoad()))();
    } finally {
      busy = false;
    }
  }
</script>

<div class={`card loaderr ${klass}`} role="alert">
  <span class="loaderr-icon"><Icon name="alert" size={16} /></span>
  <div class="min-w-0 flex-1">
    <div class="text-base font-semibold text-ink">
      {denied ? `You can't see ${what}` : `Couldn't load ${what}`}
    </div>
    {#if denied}
      <p class="mt-1 text-sm leading-[1.55] text-ink-3">
        Your role on this mesh doesn't include this. An admin can grant it under Access.
      </p>
    {/if}
    <div class="selectable mono mt-1.5 text-xs leading-relaxed break-words" style="color: var(--status-error-ink);">
      {error}
    </div>
  </div>
  <button class="btn btn-secondary btn-md flex-none" disabled={busy} onclick={again}>
    {busy ? "Retrying…" : "Retry"}
  </button>
</div>

<style>
  .loaderr {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 16px 18px;
    border-left: 3px solid var(--status-error-ink);
  }
  .loaderr-icon {
    display: grid;
    flex: none;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: var(--radius-md);
    background: var(--status-error-wash);
    color: var(--status-error-ink);
  }
</style>
