<script lang="ts">
  // "Your security key needs you" — two moments, one screen (2026-09-18).
  //
  // `start`: this account signs with a key on a security key, and the key
  // agent isn't holding it, so Director can't start. The whole window, like
  // the unlock gate, because there is nothing behind it to draw yet.
  //
  // `paused`: the key refused a signature — a prompt cancelled, a wrong
  // PIN, the key pulled out — and Director stopped asking (`KeyGate` in
  // engine.rs). Over the app, which is still there underneath.
  //
  // It replaces the thing that went wrong: a prompt that came back after
  // every Cancel, about thirty times. The tone is a pause, not an error —
  // nothing is broken, and every way forward is one button.
  import { api, errText, type SecurityKeyStatusView } from "../api";
  import { app } from "../state.svelte";
  import Icon from "./Icon.svelte";

  let { mode }: { mode: "start" | "paused" } = $props();

  let st = $state<SecurityKeyStatusView | null>(null);
  let pin = $state("");
  let busy = $state(false);
  let error = $state<string | null>(null);
  let goingBack = $state(false);

  async function load() {
    st = await api.securityKeyStatus().catch(() => null);
  }
  $effect(() => {
    void load();
  });

  // The spare a binding set aside is only worth offering when there is
  // exactly one — otherwise Settings is the place to choose.
  const spare = $derived(st?.setAside.length === 1 ? st.setAside[0] : null);
  const isFile = $derived(st?.origin === "file");

  async function tryAgain() {
    busy = true;
    error = null;
    try {
      if (mode === "start") {
        const lock = await api.identityLockStatus();
        app.lock = lock;
        if (lock.hardware && !lock.hardwareReady) {
          error = "Director still can't see your security key. Is it plugged in?";
          return;
        }
        await app.onKeyConnected();
      } else {
        await app.resumeKey();
      }
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function connect(e: Event) {
    e.preventDefault();
    if (busy || !pin) return;
    busy = true;
    error = null;
    try {
      st = await api.securityKeyConnect(pin);
      pin = "";
      if (st.reachable) await (mode === "start" ? app.onKeyConnected() : app.resumeKey());
      else error = "Your YubiKey connected, but it doesn't hold the key this account uses.";
    } catch (e2) {
      pin = "";
      error = errText(e2);
    } finally {
      busy = false;
    }
  }

  async function goBack() {
    if (!spare) return;
    busy = true;
    error = null;
    try {
      await api.securityKeyRestore(spare.name);
      sessionStorage.setItem("app:view", "settings");
      await app.restartIdentity();
    } catch (e) {
      error = errText(e);
      busy = false;
    }
  }
</script>

<div
  class={mode === "start" ? "flex h-full items-center justify-center p-7" : "fixed inset-0 z-50 flex items-center justify-center p-7"}
  style={mode === "paused" ? "background: rgba(12,16,24,.62); backdrop-filter: blur(3px);" : ""}
>
  <div
    class="on-code pop-in w-[468px] max-w-[94vw] rounded-2xl bg-code p-6 text-inkcode"
    style="box-shadow: var(--sh-modal);"
    role="dialog"
    aria-modal={mode === "paused"}
    aria-labelledby="keygate-title"
  >
    <div class="mono text-2xs tracking-[0.1em] uppercase" style="color: var(--accent-bright);">
      security key
    </div>

    <div id="keygate-title" class="mt-3 flex items-center gap-2.5 text-md font-semibold">
      <Icon name="secrets" size={16} />
      {mode === "start" ? "Connect your security key" : "Director is waiting for your key"}
    </div>

    <p class="mt-2 text-sm leading-relaxed text-inkcode-3">
      {#if mode === "start"}
        Your account signs in with your security key, and Director can't see it right now. Plug it
        in, then choose Try again.
      {:else if isFile}
        Your key agent asked for a password and didn't get one, so Director stopped asking — it
        won't keep popping up. Choose Try again when you're ready.
      {:else}
        Your security key didn't sign just now — maybe a prompt was cancelled, or the key was
        unplugged. So Director stopped asking, and won't keep popping up. Choose Try again when
        you're ready.
      {/if}
    </p>

    {#if error}
      <div class="mt-3 text-sm leading-relaxed text-error-ink" role="alert">{error}</div>
    {/if}

    <button class="btn btn-primary btn-lg mt-5 w-full" disabled={busy} onclick={tryAgain}>
      {busy && !pin ? "Checking…" : "Try again"}
    </button>

    {#if st?.canConnect && !isFile}
      <!-- A YubiKey that is plugged in but not connected to the agent: its
           PIN connects it, which is what `ssh-add -s` asks for. -->
      <form class="mt-5" onsubmit={connect}>
        <label class="block">
          <span class="text-xs text-inkcode-4">Plugged in, but not connected? Enter its PIN:</span>
          <span class="mt-1.5 flex gap-2">
            <input
              bind:value={pin}
              type="password"
              inputmode="numeric"
              autocomplete="off"
              disabled={busy}
              aria-label="Security key PIN"
              class="field h-10 min-w-0 flex-1 px-3 text-base outline-none"
            />
            <button type="submit" class="btn btn-secondary btn-lg" disabled={busy || !pin}>
              {busy && pin ? "Connecting…" : "Connect"}
            </button>
          </span>
        </label>
        <p class="mt-1.5 text-2xs leading-relaxed text-inkcode-4">
          After 3 wrong PINs in a row, a YubiKey locks its PIN — so take your time.
        </p>
      </form>
    {/if}

    {#if spare}
      <div class="mt-5 border-t pt-4" style="border-color: rgba(160, 176, 200, 0.16);">
        {#if goingBack}
          <p class="m-0 text-sm leading-relaxed text-inkcode-3">
            Director will switch back to the key you used before. If it's protected with a
            passphrase, you'll be asked for it next.
          </p>
          <div class="mt-3 flex gap-2">
            <button class="btn btn-secondary btn-lg" disabled={busy} onclick={goBack}>Switch back</button>
            <button class="btn-ghost text-sm" disabled={busy} onclick={() => (goingBack = false)}>Cancel</button>
          </div>
        {:else}
          <button class="text-sm underline text-inkcode-3" onclick={() => (goingBack = true)}>
            Or go back to the key you used before
          </button>
        {/if}
      </div>
    {/if}
  </div>
</div>
