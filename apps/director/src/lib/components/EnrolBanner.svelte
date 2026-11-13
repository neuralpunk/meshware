<script lang="ts">
  // "Request access" (M23 §3.1, §4).
  //
  // Somebody handed a Director build and an address arrives here: their key
  // is real, every request they make is signed, and every one of them is
  // denied. Without this banner that state is indistinguishable from the
  // product being broken — so it is the one state that has to explain
  // itself best, and it has to end in something a person can press.
  //
  // Pressing it authors exactly what `meshnode enrol` authors. There is no
  // password, no session, and no new authorization path: a request is a row
  // in a queue that an admin must still act on.
  import { api, errText } from "../api";
  import { app, ui } from "../state.svelte";

  const me = $derived(app.whoami);
  /** Founders sign with the daemon's own admin key and never see this. */
  const unenrolled = $derived(!!me && !me.registered && !me.isFounder);

  let username = $state("");
  let busy = $state(false);
  let err = $state<string | null>(null);
  let seeded = $state(false);

  $effect(() => {
    // Seed the name field once, from what they already asked for if they
    // have asked before.
    if (!seeded && me) {
      username = me.username ?? "";
      seeded = true;
    }
  });

  async function request() {
    busy = true;
    err = null;
    try {
      const code = await api.enrolRequest(username.trim());
      app.flash(`asked to join — your code is ${code}`);
      // Straight to "you are in the queue", without waiting for the poll.
      await app.refreshWhoami();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }
</script>

{#if unenrolled && me}
  <div
    class="flex-none px-[22px] py-3 text-sm"
    style="background: var(--status-warn-wash); color: var(--status-warn-ink); border-bottom: 1px solid var(--edge);"
  >
    {#if me.enrolment === "pending"}
      <!-- Say where they already are, not just that they are waiting. The
           previous copy left somebody unable to tell whether an admin had
           been told at all, or whether they were supposed to do something
           else next. -->
      <div class="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <span class="font-semibold">You are in the queue.</span>
        <span>
          Any admin of this mesh can see you at the top of their Access page right now. If they
          ask you to read out a code, it is this one:
        </span>
        <span
          class="mono rounded-full px-2.5 py-0.5 text-xs font-semibold"
          style="background: var(--surface); color: var(--ink-2);"
        >
          {me.enrolmentCode}
        </span>
        <span class="text-xs">
          You are <span class="mono">{me.userId.slice(0, 14)}…</span> on
          <span class="mono">{me.nodeName}</span>.
        </span>
      </div>
    {:else}
      <div class="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <span class="font-semibold">
          {#if me.enrolment === "declined"}
            A mesh admin declined your request.
          {:else if me.enrolment === "expired"}
            Nobody answered your request and it expired.
          {:else}
            This mesh does not know your key yet.
          {/if}
        </span>
        <span>
          There is no password to set — you <em>are</em> a key on this machine
          (<span class="mono">{me.userId.slice(0, 14)}…</span> on <span class="mono">{me.nodeName}</span>).
          Ask an admin to recognise it.
        </span>
        <label class="ml-auto flex items-center gap-2">
          <span class="text-xs font-semibold">Call me</span>
          <input
            bind:value={username}
            placeholder="your name"
            class="mono h-8 w-40 rounded-md border px-2.5 text-xs"
            style="border-color: var(--edge); background: var(--surface); color: var(--ink-2);"
          />
        </label>
        <button class={ui.rowAction} disabled={busy} onclick={request}>
          {busy ? "Asking…" : me.enrolment === "declined" ? "Ask again" : "Request access"}
        </button>
      </div>
    {/if}
    {#if err}<div class="mt-1.5 text-xs" style="color: var(--status-error-ink);">{err}</div>{/if}
  </div>
{/if}
