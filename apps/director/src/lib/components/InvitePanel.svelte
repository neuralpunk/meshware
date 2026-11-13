<script lang="ts">
  // "Invite someone" — the way in for a person who is not on your network.
  //
  // LAN discovery covers the case where two machines can see each other,
  // and it covers it well enough that it should be the default answer. It
  // cannot cover a colleague at home, a cloud VM, or an office that drops
  // multicast — and the previous answer for all three was a token minted
  // three screens away under "Add node", with no accompanying sentence
  // about what the person receiving it was supposed to do with it.
  //
  // So this panel is mostly *instructions*, with a token stapled to them.
  // The thing an admin actually wants to produce here is a message they can
  // paste into a chat window, so that is what the button copies.
  import { api, errText, type InviteView } from "../api";
  import { app, ui, until } from "../state.svelte";
  import Icon from "./Icon.svelte";

  let { onclose }: { onclose: () => void } = $props();

  let ttlMins = $state(60);
  let uses = $state(1);
  let token = $state<string | null>(null);
  let busy = $state(false);
  let err = $state<string | null>(null);

  // The invites still able to let somebody in, so a token sent to the
  // wrong chat can be taken back without waiting for it to expire.
  let invites = $state<InviteView[]>([]);
  let confirming = $state<string | null>(null);
  const open = $derived(
    invites.filter((i) => !i.revoked && i.used < i.uses && i.expiresAtMicros > Date.now() * 1000),
  );

  async function loadInvites() {
    try {
      invites = await api.invites();
    } catch {
      // Not an admin, or the engine is busy: the list is a convenience,
      // and the panel works without it.
      invites = [];
    }
  }

  $effect(() => {
    loadInvites();
  });

  async function revoke(invite: string) {
    if (confirming !== invite) {
      confirming = invite;
      return;
    }
    busy = true;
    err = null;
    try {
      await api.inviteRevoke(invite);
      app.flash("Invite revoked");
      if (invite === token) token = null;
      confirming = null;
      await loadInvites();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  const meshName = $derived(app.status?.meshName ?? "the mesh");

  /** The message an admin pastes to the person joining. */
  const message = $derived(
    token
      ? `You've been invited to the "${meshName}" Meshware mesh.

1. Install Meshware Director and open it.
2. On the welcome screen choose "Join with an invite".
3. Paste this in:

${token}

It works ${uses === 1 ? "once" : `${uses} times`} and expires in ${ttlMins >= 60 ? `${Math.round(ttlMins / 60)} hour${ttlMins >= 120 ? "s" : ""}` : `${ttlMins} minutes`}.
Once you're in, I'll get a request to approve and you'll be able to use the mesh.`
      : "",
  );

  async function mint() {
    busy = true;
    err = null;
    try {
      token = await api.inviteCreate(ttlMins * 60, uses, []);
      confirming = null;
      await loadInvites();
    } catch (e) {
      err = errText(e);
    } finally {
      busy = false;
    }
  }

  async function copy(text: string, what: string) {
    try {
      await api.copyText(text);
      app.flash(`${what} copied`);
    } catch {
      app.flash("Could not reach the clipboard");
    }
  }
</script>

<div class="fade-in card p-[22px]">
  <div class="flex flex-wrap items-baseline gap-3">
    <h2 class="m-0 text-lg font-bold tracking-[-.02em] text-ink">Invite someone</h2>
    <span class="text-sm text-ink-4">for a person who is not on your network</span>
    <button class="ml-auto text-sm font-semibold text-ink-4 hover:text-ink" onclick={onclose}>Close</button>
  </div>

  <p class="mt-2 mb-0 max-w-3xl text-sm leading-relaxed text-ink-4">
    If their machine is on this network you do not need this — start Meshware Director there,
    pick <strong>Pair on this network</strong>, and they will appear at the top of this page on
    their own. An invite is for everybody else: a laptop at home, a cloud machine, an office
    that blocks discovery.
  </p>

  {#if !token}
    <div class="mt-4 flex flex-wrap items-end gap-3">
      <label class="block w-44">
        <span class={ui.label}>Good for</span>
        <select bind:value={ttlMins} class={`${ui.input} select`}>
          <option value={15}>15 minutes</option>
          <option value={60}>1 hour</option>
          <option value={480}>8 hours</option>
          <option value={1440}>24 hours</option>
        </select>
      </label>
      <label class="block w-44">
        <span class={ui.label}>How many people</span>
        <select bind:value={uses} class={`${ui.input} select`}>
          <option value={1}>Just one</option>
          <option value={5}>Up to 5</option>
          <option value={25}>Up to 25</option>
        </select>
      </label>
      <button class={ui.primary} disabled={busy} onclick={mint}>
        {busy ? "Creating…" : "Create invite"}
      </button>
    </div>
    <p class="mt-3 mb-0 text-xs leading-relaxed text-ink-5">
      An invite lets a machine join. It does <strong>not</strong> decide what the person on it can
      do — they will still turn up at the top of this page for you to name and give a role to.
    </p>
    {#if open.length > 0}
      <div class="mt-5">
        <div class="label">Invites that still work</div>
        <ul class="m-0 mt-1.5 list-none p-0">
          {#each open as inv (inv.inviteId)}
            <li class="flex flex-wrap items-center gap-3 border-t border-edge py-2 text-sm">
              <span class="font-mono text-xs text-ink-3">{inv.inviteId}</span>
              <span class="text-ink-4">
                {inv.used} of {inv.uses} used · expires {until(inv.expiresAtMicros)}
                {inv.createdBy ? ` · by ${inv.createdBy}` : ""}
              </span>
              <button class={`${ui.rowDanger} ml-auto`} disabled={busy} onclick={() => revoke(inv.inviteId)}>
                {confirming === inv.inviteId ? "Revoke — sure?" : "Revoke"}
              </button>
            </li>
          {/each}
        </ul>
      </div>
    {/if}
  {:else}
    <div class="fade-in mt-4">
      <div class="label">Send them this</div>
      <!-- `overflow-wrap: anywhere` rather than `break-words`: the token is
           one 180-character run with no break opportunity in it, and
           without this it pushes the whole page sideways. -->
      <div
        class="selectable mt-1.5 rounded-lg px-4 py-3.5 text-sm leading-relaxed whitespace-pre-wrap text-ink-2"
        style="background: var(--surface-2); overflow-wrap: anywhere;"
      >{message}</div>
      <div class="mt-3 flex flex-wrap items-center gap-2">
        <button class={ui.primary} onclick={() => copy(message, "Invite message")}>
          <span class="inline-flex items-center gap-2"><Icon name="copy" size={14} /> Copy the whole message</span>
        </button>
        <button class={ui.secondary} onclick={() => copy(token ?? "", "Invite")}>Copy just the invite</button>
        <button class={ui.secondary} onclick={() => (token = null)}>Create another</button>
        <button class={`${ui.rowDanger} ml-auto`} disabled={busy} onclick={() => revoke(token ?? "")}>
          {confirming === token ? "Revoke — sure?" : "Revoke this invite"}
        </button>
      </div>
      <p class="mt-3 mb-0 text-xs leading-relaxed text-ink-5">
        The invite is not a password and carries no permissions — it lets a machine reach this
        mesh and be recognised. Anyone holding it before it expires can join a machine, so send
        it the way you would send a door code — and if it went somewhere it should not have,
        revoke it.
      </p>
    </div>
  {/if}

  {#if err}<div class={ui.errorBox}>{err}</div>{/if}
</div>
