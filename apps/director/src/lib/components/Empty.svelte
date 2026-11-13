<script lang="ts">
  // An empty state that can be acted on (L1 P4).
  //
  // The old pattern was a title and a reason. A reason is not a next step,
  // and "Nothing has run yet" told a person what they already knew. This
  // one adds three things, in this order:
  //
  //   1. **One action.** Exactly one — an empty view with three buttons is
  //      a decision, and a decision is what somebody who has just arrived
  //      does not have the information to make.
  //   2. **The command it is equivalent to**, in mono, click to copy. The
  //      button fills the view; the command teaches the product. A person
  //      who reads it once has learned the CLI without being sent to it.
  //   3. **The reassurance line** — what it touches, roughly how long it
  //      takes, and that it changes nothing. This is the one that gets
  //      skipped and the one that makes the button pressable: nobody runs
  //      a command against machines they own on the word of a button.
  //
  // Still typographic, still no illustration — the design pass's rule for
  // empty states (`DIRECTION-2026-09-commercial.md` §6) is unchanged. A
  // button is not an illustration.
  import type { Snippet } from "svelte";
  import { api, errText } from "../api";
  import { app } from "../state.svelte";

  let {
    title,
    sub,
    action,
    command,
    reassurance,
    busy = false,
    error = null,
    children,
  }: {
    title: string;
    /** Why the view is empty, in one sentence. */
    sub?: string;
    /** The one safe, correct next thing. Omit for a state that is a
     *  statement rather than a gap — "every machine is on the same build"
     *  is news, not a to-do. */
    action?: { label: string; run: () => void };
    /** The CLI that does the same thing. */
    command?: string;
    /** What it touches · how long · what it changes. */
    reassurance?: string;
    busy?: boolean;
    /**
     * Why the last press produced nothing. It belongs *here*, under the
     * button, and not in a toast: a flash that has already faded is
     * indistinguishable from a button that does not work, and an empty
     * state is precisely where somebody has no other evidence to go on.
     */
    error?: string | null;
    /** Richer prose than `sub`, when a link belongs in it. */
    children?: Snippet;
  } = $props();

  async function copy() {
    if (!command) return;
    try {
      await api.copyText(command);
      app.flash("copied");
    } catch (e) {
      app.flash(errText(e));
    }
  }
</script>

<div class="card empty">
  <div class="empty-title">{title}</div>
  {#if sub}<div class="empty-sub">{sub}</div>{/if}
  {#if children}<div class="empty-sub">{@render children()}</div>{/if}

  {#if action}
    <div class="mt-5 flex flex-col items-center gap-2.5">
      <button class="btn btn-primary btn-md" disabled={busy} onclick={action.run}>
        {busy ? "Working…" : action.label}
      </button>
      {#if error}
        <div
          class="selectable mx-auto max-w-md rounded-md px-3 py-2 text-xs leading-relaxed"
          style="background: var(--status-warn-wash); color: var(--status-warn-ink);"
          role="status"
        >
          {error}
        </div>
      {:else if reassurance}
        <div class="text-xs text-ink-5">{reassurance}</div>
      {/if}
      {#if command}
        <button
          class="mono rounded-md px-2.5 py-1 text-xs transition-colors"
          style="background: var(--surface-inline); color: var(--ink-4);"
          title="Copy this command"
          onclick={copy}
        >
          {command}
        </button>
      {/if}
    </div>
  {/if}
</div>
