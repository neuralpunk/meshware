<script lang="ts">
  // The screen a sealed key gets, before anything else runs (B9).
  //
  // Everything Director does needs the user key: `bootstrap_status` reads
  // it, `start_engine` signs with it. So this is not a modal over the app —
  // there is no app behind it yet. It is the whole window until the key
  // opens, which is why it borrows the shape of the `fatal` card in
  // App.svelte rather than Modal.svelte's overlay.
  //
  // It is deliberately *not* styled as an error. A locked key is the
  // feature working. The tone to hit is a door, not a crash.
  import { api, errText, type IdentityLockView } from "../api";
  import Icon from "./Icon.svelte";

  let { lock, onunlocked }: {
    lock: IdentityLockView;
    onunlocked: (next: IdentityLockView) => void;
  } = $props();

  let passphrase = $state("");
  let busy = $state(false);
  let error = $state<string | null>(null);
  let input = $state<HTMLInputElement | null>(null);

  // The only thing on screen is one field. Focusing anything else would be
  // a decision to make somebody click first.
  $effect(() => {
    input?.focus();
  });

  async function submit(e: Event) {
    e.preventDefault();
    if (busy || !passphrase) return;
    busy = true;
    error = null;
    try {
      const next = await api.identityUnlock(passphrase);
      // Wiped from the component the moment it is no longer needed. It has
      // already been through a JSON boundary and this does not undo that —
      // it just declines to keep a copy for the life of the window.
      passphrase = "";
      onunlocked(next);
    } catch (err) {
      error = errText(err);
      passphrase = "";
      input?.focus();
    } finally {
      busy = false;
    }
  }
</script>

<div class="flex h-full items-center justify-center p-7">
  <form
    onsubmit={submit}
    class="on-code pop-in w-[452px] max-w-[94vw] rounded-2xl bg-code p-6 text-inkcode"
    style="box-shadow: var(--sh-modal);"
  >
    <div class="mono text-2xs tracking-[0.1em] uppercase" style="color: var(--accent-bright);">
      identity locked
    </div>

    <div class="mt-3 flex items-center gap-2.5 text-md font-semibold">
      <Icon name="secrets" size={16} />
      Unlock your key
    </div>

    <p class="mt-2 text-sm leading-relaxed text-inkcode-3">
      Your key is protected with a passphrase. Director needs it open to sign anything —
      it stays open for this login session.
    </p>

    <!-- `.field`, not `.field-sunken`. Sunken paints with `--surface-subtle`,
         the one token `.on-code` does not re-point, so it renders
         light-theme white on this dark card. `.field` uses `--surface-1`,
         which is re-pointed. -->
    <label class="mt-5 block">
      <span class="mono text-2xs tracking-[0.08em] uppercase text-inkcode-4">passphrase</span>
      <input
        bind:this={input}
        bind:value={passphrase}
        type="password"
        autocomplete="current-password"
        disabled={busy}
        class="field mt-1.5 h-10 w-full px-3 text-base outline-none"
      />
    </label>

    {#if error}
      <!-- role=alert so the failure is announced, not just recoloured: this
           screen has one control and a person may not be looking at it. -->
      <div class="selectable mono mt-3 text-sm leading-relaxed text-error-ink" role="alert">
        {error}
      </div>
    {/if}

    <button
      type="submit"
      disabled={busy || !passphrase}
      class="btn btn-primary btn-lg mt-5 w-full"
    >
      {busy ? "Unlocking…" : "Unlock"}
    </button>

    <p class="mono mt-4 text-2xs leading-relaxed text-inkcode-4">
      {#if lock.sessionStore}
        <!-- Naming the file is not decoration. "Where did my unlock go?" is
             answerable by looking, and this is the only place that says. -->
        Kept until you log out, or 12 hours, in
        <span class="selectable">{lock.sessionStore}</span>
      {:else}
        There is no <span class="selectable">$XDG_RUNTIME_DIR</span> here, so an unlock cannot be
        remembered for the session — Director will ask again next time.
      {/if}
    </p>

    <p class="mono mt-2 text-2xs leading-relaxed text-inkcode-4">
      Same key and same unlock as the CLI: <span class="selectable">meshnode identity unlock</span>
      in a terminal does this too.
    </p>
  </form>
</div>
