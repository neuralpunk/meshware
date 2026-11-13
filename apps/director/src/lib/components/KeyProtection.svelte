<script lang="ts">
  // Settings → this peer → "Key protection" (B9, and rule D-E3).
  //
  // The unlock gate handles the one moment Director cannot start without.
  // This is the rest of the lifecycle — protect, lock, unprotect — so the
  // capability is not CLI-only. D-E3 is the rule that a thing landing in
  // only one of the two front ends is a thing half the product does not
  // have, and a key you can protect but only from a terminal is exactly
  // that shape.
  //
  // Its own component rather than another 120 lines in Settings.svelte,
  // which is already the longest view in the tree.
  import { api, errText, fmtRoughDuration, type IdentityLockView } from "../api";
  import { app, ui } from "../state.svelte";
  import Hint from "./Hint.svelte";

  // The window's copy, not one of our own: the Security key card below
  // can change what this file is, and a panel still calling it a
  // plaintext seed afterwards would be describing a file that is gone.
  const lock = $derived(app.lock);
  let busy = $state(false);
  let error = $state<string | null>(null);

  // Which form is open, if any. One at a time: these are three different
  // acts on one file and stacking them would invite doing two by accident.
  let mode = $state<"idle" | "protect" | "unprotect">("idle");
  let pass = $state("");
  let confirm = $state("");

  async function refresh() {
    try {
      app.lock = await api.identityLockStatus();
    } catch (e) {
      error = errText(e);
    }
  }

  $effect(() => {
    if (!lock) void refresh();
  });

  function reset() {
    mode = "idle";
    pass = "";
    confirm = "";
    error = null;
  }

  async function run(fn: () => Promise<IdentityLockView>) {
    busy = true;
    error = null;
    try {
      app.lock = await fn();
      reset();
    } catch (e) {
      error = errText(e);
      // The passphrase is cleared on failure but the form stays open: the
      // likely cause is a typo, and closing it would make them start over.
      pass = "";
      confirm = "";
    } finally {
      busy = false;
    }
  }

  const mismatch = $derived(mode === "protect" && !!pass && !!confirm && pass !== confirm);
</script>

<div class={ui.card}>
  <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
    Key protection
    <Hint
      text="Your key is 32 bytes and it is your whole identity. Protecting it encrypts the file with a passphrase, so a backup, a sync client or a stolen disk gets ciphertext. It does not protect it from something running as you while it is unlocked."
    />
  </h2>

  {#if !lock}
    <p class="mt-2 mb-0 text-sm text-ink-4">Reading…</p>
  {:else}
    <dl class="mt-4 space-y-3.5 text-sm">
      <div>
        <dt class="label">Key file</dt>
        <dd class="mono selectable mt-1 ml-0 break-all text-2xs text-ink-5">{lock.path}</dd>
      </div>
      <div>
        <dt class="label">State</dt>
        <dd class="mt-1 ml-0 text-ink-2">
          {#if lock.hardware}
            Held by your key agent — this file only points at it, so there's nothing here to
            protect with a passphrase. See Security key, below.
          {:else if !lock.protected}
            Unprotected — a plaintext 32-byte seed, mode 0600
          {:else if lock.unlocked}
            Protected · <span style="color: var(--accent-ink);">unlocked</span> for {fmtRoughDuration(
              Number(lock.unlockedForSecs ?? 0),
            )}
          {:else}
            Protected · locked
          {/if}
        </dd>
      </div>
      {#if lock.protected && !lock.sessionStore}
        <div>
          <dt class="label">Session</dt>
          <dd class="mt-1 ml-0 text-xs" style="color: var(--status-warn-ink);">
            No <span class="mono">$XDG_RUNTIME_DIR</span> on this machine, so an unlock cannot be
            kept for the session — every start will ask again.
          </dd>
        </div>
      {/if}
    </dl>

    {#if lock.hardware}
      <!-- Y2/Y3. The Security key card below is where this key is managed;
           a passphrase has nothing to seal. -->
    {:else if mode === "idle"}
      <div class="mt-4 flex flex-wrap gap-2">
        {#if !lock.protected}
          <button class={ui.secondary} disabled={busy} onclick={() => (mode = "protect")}>
            Protect with a passphrase…
          </button>
        {:else}
          {#if lock.unlocked}
            <button
              class={ui.secondary}
              disabled={busy}
              title="forget the unlock now, without waiting for it to expire"
              onclick={() => run(api.identityLockNow)}
            >
              Lock now
            </button>
          {/if}
          <button class={ui.secondary} disabled={busy} onclick={() => (mode = "unprotect")}>
            Remove protection…
          </button>
        {/if}
      </div>
    {:else if mode === "protect"}
      <!-- Typed twice. A key sealed under a typo is a key nobody owns any
           more: there is no recovery path and no copy of the plaintext. -->
      <div class="mt-4 space-y-3">
        <label class="block">
          <span class={ui.label}>New passphrase</span>
          <input bind:value={pass} type="password" autocomplete="new-password" class={ui.input} />
        </label>
        <label class="block">
          <span class={ui.label}>Again</span>
          <input
            bind:value={confirm}
            type="password"
            autocomplete="new-password"
            class={ui.input}
          />
        </label>
        {#if mismatch}
          <p class="m-0 text-xs" style="color: var(--status-warn-ink);">
            Those do not match.
          </p>
        {/if}
        <p class="m-0 text-xs leading-relaxed text-ink-4">
          There is no way to recover this. If you lose it, an admin has to rotate you back in
          with <span class="mono">meshnode users rotate</span>.
        </p>
        <div class="flex gap-2">
          <button
            class={ui.primary}
            disabled={busy || !pass || pass !== confirm}
            onclick={() => run(() => api.identityProtect(pass))}
          >
            {busy ? "Protecting…" : "Protect"}
          </button>
          <button class={ui.secondary} disabled={busy} onclick={reset}>Cancel</button>
        </div>
      </div>
    {:else}
      <div class="mt-4 space-y-3">
        <p class="m-0 text-sm leading-relaxed text-ink-3">
          This writes your key back as an unencrypted file. Your passphrase is still needed to do
          it, so nobody can do this at a screen you left unlocked.
        </p>
        <label class="block">
          <span class={ui.label}>Passphrase</span>
          <input
            bind:value={pass}
            type="password"
            autocomplete="current-password"
            class={ui.input}
          />
        </label>
        <div class="flex gap-2">
          <button
            class={ui.secondary}
            disabled={busy || !pass}
            onclick={() => run(() => api.identityUnprotect(pass))}
          >
            {busy ? "Removing…" : "Remove protection"}
          </button>
          <button class={ui.secondary} disabled={busy} onclick={reset}>Cancel</button>
        </div>
      </div>
    {/if}

    {#if error}
      <div class={ui.errorBox} role="alert">{error}</div>
    {/if}
  {/if}
</div>
