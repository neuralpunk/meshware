<script lang="ts">
  // Settings → this machine → "Security key" (Y2/Y3, rule D-E3).
  //
  // Rewritten 2026-09-18 after it went wrong for a real person. They chose
  // what they thought was their YubiKey; it was a key file in ~/.ssh
  // (GNOME's agent lists every one), the card then said "on a security
  // key", and the agent asked for that file's passphrase on every refresh
  // until about thirty prompts had been dismissed. And it read like a
  // manual. So now:
  //
  //   - one plain sentence says where the key is — honestly: a key an
  //     agent holds is not called a security key unless it looks like one,
  //     and a key file is called a key file;
  //   - setting up is a few short steps, and key files can't be picked;
  //   - the key signs twice before anything changes, which catches a key
  //     that would ask for its PIN on every refresh;
  //   - a YubiKey can be connected with its PIN, here;
  //   - and there is always a way back to the key you had.
  //
  // The rules are not here. What may be bound is `mesh_core::hardware`,
  // the same code the CLI runs; this card shows the plan and asks first.
  import {
    api,
    errText,
    type SecurityKeyStatusView,
    type SecurityKeyPlanView,
    type SecurityKeyBoundView,
    type SecurityKeyTestView,
    type PickedView,
  } from "../api";
  import { app, ago, ui } from "../state.svelte";
  import Hint from "./Hint.svelte";

  let st = $state<SecurityKeyStatusView | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  // The setup steps: where they are, and what each learned.
  type Step = "closed" | "choose" | "checking" | "confirm" | "done";
  let step = $state<Step>("closed");
  let chosen = $state<string | null>(null);
  let test = $state<SecurityKeyTestView | null>(null);
  let plan = $state<SecurityKeyPlanView | null>(null);
  let bound = $state<SecurityKeyBoundView | null>(null);
  let pin = $state("");

  // Going back to the key you had.
  let goingBack = $state(false);
  let removeMoved = $state(true);

  // The optional proof.
  let proofOpen = $state(false);
  let kind = $state("piv");
  let slot = $state("9a");
  let certs = $state<PickedView[]>([]);
  let withdrawing = $state(false);

  async function refresh() {
    try {
      st = await api.securityKeyStatus();
    } catch (e) {
      error = errText(e);
    }
  }

  $effect(() => {
    void refresh();
    void finishGoingBack();
  });

  // While setting up, notice a key appearing in the agent without anyone
  // pressing anything. Listing keys never asks the key to sign, so this
  // can't raise a prompt.
  $effect(() => {
    if (step !== "choose") return;
    const timer = setInterval(() => void refresh(), 4000);
    return () => clearInterval(timer);
  });

  const me = $derived(app.users.find((u) => u.userId === app.whoami?.userId) ?? null);
  const proof = $derived(me?.hardware && !me.hardware.revoked ? me.hardware : null);
  /** Keys worth offering: not the one in use, and not a file on this computer. */
  const candidates = $derived(st?.keys.filter((k) => !k.current && k.origin !== "file") ?? []);
  const fileKeys = $derived(st?.keys.filter((k) => k.origin === "file").length ?? 0);
  const onKey = $derived(st?.held === "hardware");
  const onFile = $derived(onKey && st?.origin === "file");
  const spare = $derived(st?.setAside[st.setAside.length - 1] ?? null);
  /** The mesh account the current key is, if going back would leave it behind. */
  const leftBehind = $derived(
    onKey && st?.identity
      ? (app.users.find((u) => u.userId === st?.identity && !u.isFounder && !u.revoked) ?? null)
      : null,
  );

  function startSetup() {
    error = null;
    bound = null;
    plan = null;
    test = null;
    chosen = null;
    step = "choose";
  }

  function closeSetup() {
    step = "closed";
    plan = null;
    test = null;
    chosen = null;
    error = null;
  }

  async function choose(key: string) {
    chosen = key;
    error = null;
    test = null;
    plan = null;
    step = "checking";
    try {
      test = await api.securityKeyTest(key);
      if (!test.ok) return;
      plan = await api.securityKeyPlan(key);
      step = "confirm";
    } catch (e) {
      error = errText(e);
    }
  }

  async function carryOut() {
    if (!plan) return;
    busy = true;
    error = null;
    try {
      bound = await api.securityKeyUse(plan.key);
      step = "done";
      await app.identityChanged();
      await refresh();
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
      if (app.keyPaused !== null) await app.resumeKey();
    } catch (e2) {
      pin = "";
      error = errText(e2);
    } finally {
      busy = false;
    }
  }

  async function copy(text: string) {
    await api.copyText(text).catch(() => {});
    app.flash("Copied");
  }

  // -- going back ------------------------------------------------------------

  async function goBack() {
    if (!spare) return;
    busy = true;
    error = null;
    try {
      // Removing the account left behind is signed by the key that comes
      // back, so it happens after the restart — remembered until then.
      if (removeMoved && leftBehind) {
        sessionStorage.setItem(
          "securitykey:remove",
          JSON.stringify({ userId: leftBehind.userId, username: leftBehind.username }),
        );
      }
      await api.securityKeyRestore(spare.name);
      sessionStorage.setItem("app:view", "settings");
      await app.restartIdentity();
    } catch (e) {
      sessionStorage.removeItem("securitykey:remove");
      error = errText(e);
      busy = false;
    }
  }

  async function finishGoingBack() {
    const pending = sessionStorage.getItem("securitykey:remove");
    if (!pending || !app.ready) return;
    sessionStorage.removeItem("securitykey:remove");
    try {
      const { userId, username } = JSON.parse(pending) as { userId: string; username: string };
      await api.userRevoke(userId, "no longer needed after going back to the previous key");
      app.flash(`Switched back to your previous key, and removed “${username}”`);
      app.invalidate();
      void app.refreshExtended();
    } catch (e) {
      error = `You're back on your previous key, but removing the extra account didn't work: ${errText(e)}`;
    }
  }

  // -- the proof -------------------------------------------------------------

  async function pickCert() {
    const picked = await api
      .pickFile(certs.length ? "The intermediate certificate (DER)" : "Your key's certificate (DER)", ["der", "crt", "cer"])
      .catch((e) => {
        error = errText(e);
        return null;
      });
    if (picked) certs = [...certs, picked];
  }

  async function publish() {
    busy = true;
    error = null;
    try {
      await api.securityKeyAttest(
        certs.map((c) => c.token),
        kind,
        slot,
      );
      proofOpen = false;
      certs = [];
      app.flash("Proof published — your mesh can now see your key is on a real security key");
      app.invalidate();
      void app.refreshExtended();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function withdraw() {
    busy = true;
    error = null;
    try {
      await api.securityKeyWithdraw(null, "withdrawn from Director");
      withdrawing = false;
      app.invalidate();
      void app.refreshExtended();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  const SETUP = [
    "ykman piv keys generate -a ed25519 --pin-policy once --touch-policy never 9a meshware-key.pem",
    'ykman piv certificates generate -s "CN=meshware" -d 3650 9a meshware-key.pem',
  ].join("\n");
  const PROOF = [
    "ykman piv keys attest 9a --format DER leaf.der",
    "ykman piv certificates export f9 --format DER intermediate.der",
  ].join("\n");
</script>

<div class={ui.card}>
  <h2 class="m-0 text-md font-bold tracking-[-.015em] text-ink">
    Security key
    <Hint
      text="A security key, like a YubiKey, keeps your meshware key on the device itself. Nothing on this computer can copy it — anything that wants to act as you needs the key plugged in."
    />
  </h2>

  {#if !st}
    <p class="mt-2 mb-0 text-sm text-ink-4">Reading…</p>
  {:else}
    <!-- ═══ where your key is, in one sentence ═══ -->
    <div class="mt-3 text-sm leading-relaxed">
      {#if st.paused}
        <div class="font-medium text-ink">Director is waiting for your key</div>
        <div class="text-ink-4">
          It didn't sign just now, so Director stopped asking rather than keep popping up.
        </div>
        <button class="{ui.secondary} mt-3" disabled={busy} onclick={() => app.resumeKey().then(refresh)}>
          Try again
        </button>
      {:else if onFile}
        <div class="font-medium text-ink">You're using a key file, not a security key</div>
        <div class="text-ink-4">
          Director signs with <span class="mono">{st.file}</span>, an ordinary key file on this
          computer. Everything still works — it just isn't protected the way a security key would
          be. (An earlier version of Director didn't tell the two apart. Sorry about that.)
        </div>
      {:else if onKey && !st.reachable}
        <div class="font-medium text-ink">Your security key isn't connected</div>
        <div class="text-ink-4">Plug it in{st.canConnect ? ", then enter its PIN below" : ""}.</div>
      {:else if onKey}
        <div class="font-medium text-ink">
          <span style="color: var(--accent-ink);">✓</span> You're using your security key
        </div>
        <div class="text-ink-4">
          {#if proof}
            Proven to be on a real security key ({proof.kind.toUpperCase()}, vouched for by
            {proof.rootLabel}).
          {:else if st.origin === "device"}
            Your key agent loads it from your security key.
          {:else}
            Your key agent holds it, and it isn't a file on this computer.
          {/if}
        </div>
      {:else}
        <div class="font-medium text-ink">Your key is saved on this computer</div>
        <div class="text-ink-4">
          {st.held === "sealed" ? "It's protected with a passphrase. " : ""}A security key keeps it
          off this computer entirely, so it can't be copied.
        </div>
      {/if}
    </div>

    {#if step === "closed"}
      <div class="mt-4 flex flex-wrap items-center gap-2">
        {#if !onKey || onFile}
          <button class={ui.primary} onclick={startSetup}>Set up a security key</button>
        {/if}
        {#if spare && onKey}
          <button class={ui.secondary} onclick={() => (goingBack = !goingBack)}>
            Go back to your previous key
          </button>
        {/if}
      </div>
    {/if}

    <!-- Connect with a PIN: a YubiKey that is plugged in but not loaded. -->
    {#if onKey && !onFile && !st.reachable && st.canConnect && step === "closed"}
      <form class="mt-4 flex max-w-sm gap-2" onsubmit={connect}>
        <input
          bind:value={pin}
          type="password"
          inputmode="numeric"
          autocomplete="off"
          placeholder="YubiKey PIN"
          aria-label="YubiKey PIN"
          disabled={busy}
          class={ui.input}
        />
        <button type="submit" class={ui.secondary} disabled={busy || !pin}>
          {busy ? "Connecting…" : "Connect"}
        </button>
      </form>
    {/if}

    <!-- ═══ going back ═══ -->
    {#if goingBack && spare}
      <div class="mt-4 rounded-md bg-surface-2 px-4 py-3.5 text-sm leading-relaxed text-ink-3">
        <p class="m-0">
          Director will switch back to the key you used before, which it kept safe when you moved.
          If that key has a passphrase, you'll be asked for it next.
        </p>
        {#if leftBehind}
          <label class="mt-2.5 flex items-start gap-2 text-sm">
            <input type="checkbox" bind:checked={removeMoved} class="mt-1" />
            <span>
              Also remove <strong>{leftBehind.username}</strong> from your mesh — it was added for
              the key you're leaving.
            </span>
          </label>
        {/if}
        <div class="mt-3 flex gap-2">
          <button class={ui.primary} disabled={busy} onclick={goBack}>
            {busy ? "Switching…" : "Switch back"}
          </button>
          <button class={ui.secondary} disabled={busy} onclick={() => (goingBack = false)}>Cancel</button>
        </div>
      </div>
    {/if}

    <!-- ═══ setting up ═══ -->
    {#if step !== "closed"}
      <div class="mt-4 rounded-md bg-surface-2 px-4 py-4 text-sm leading-relaxed text-ink-3">
        {#if step === "choose"}
          {#if candidates.length === 0}
            <div class="font-medium text-ink">1. Put a key on your YubiKey</div>
            <p class="mt-1 mb-0">
              Your YubiKey needs a key made just for meshware. You only do this once, in a
              terminal — copy these two lines and run them. It asks for your YubiKey's PIN, and for
              its management key (press Enter if you've never changed it).
            </p>
            <pre class="mono selectable mt-2 mb-0 overflow-x-auto rounded bg-surface px-3 py-2 text-2xs text-ink-2">{SETUP}</pre>
            <div class="mt-1.5 flex items-center gap-3">
              <button class="linkish text-xs" onclick={() => copy(SETUP)}>Copy</button>
              <span class="text-xs text-ink-5">
                This uses slot 9a. If you already use 9a for something else, don't run it.
              </span>
            </div>

            <div class="mt-4 font-medium text-ink">2. Connect it</div>
            {#if st.canConnect}
              <form class="mt-1.5 flex max-w-sm gap-2" onsubmit={connect}>
                <input
                  bind:value={pin}
                  type="password"
                  inputmode="numeric"
                  autocomplete="off"
                  placeholder="YubiKey PIN"
                  aria-label="YubiKey PIN"
                  disabled={busy}
                  class={ui.input}
                />
                <button type="submit" class={ui.secondary} disabled={busy || !pin}>
                  {busy ? "Connecting…" : "Connect"}
                </button>
              </form>
              <p class="mt-1.5 mb-0 text-xs text-ink-5">
                After 3 wrong PINs in a row, a YubiKey locks its PIN — so take your time.
              </p>
            {:else}
              <p class="mt-1 mb-0">
                Director needs OpenSC to talk to your YubiKey. Install the <span class="mono">opensc</span>
                package, then come back — this page will notice.
              </p>
            {/if}
            <p class="mt-3 mb-0 text-xs text-ink-5">Waiting for your key to show up…</p>
          {:else}
            <div class="font-medium text-ink">Choose your security key</div>
            <div class="mt-2 flex flex-col gap-2">
              {#each candidates as k (k.userId)}
                <div class="flex items-center gap-3 rounded-md bg-surface px-3 py-2">
                  <span class="min-w-0">
                    <span class="block text-sm text-ink-2">{k.comment || "Security key"}</span>
                    <span class="mono block text-2xs text-ink-5">{k.shortId}</span>
                  </span>
                  <button class="{ui.rowAction} ml-auto" onclick={() => choose(k.userId)}>Use this key</button>
                </div>
              {/each}
            </div>
          {/if}
          {#if fileKeys > 0}
            <p class="mt-3 mb-0 text-xs text-ink-5">
              Keys saved as files on this computer aren't listed — they wouldn't protect anything.
            </p>
          {/if}
          <button class="linkish mt-3 text-xs" onclick={closeSetup}>Cancel</button>
        {:else if step === "checking"}
          {#if !test && !error}
            <div class="font-medium text-ink">Checking your key…</div>
            <p class="mt-1 mb-0">It may ask for your PIN or a touch. Nothing changes yet.</p>
          {:else}
            <div class="font-medium text-ink">{test?.ok ? "Your key works" : "That didn't work"}</div>
            <p class="mt-1 mb-0">{error ?? test?.message}</p>
            {#if test?.asksEveryTime}
              <pre class="mono selectable mt-2 mb-0 overflow-x-auto rounded bg-surface px-3 py-2 text-2xs text-ink-2">{SETUP}</pre>
              <button class="linkish mt-1 text-xs" onclick={() => copy(SETUP)}>Copy</button>
              <p class="mt-1 mb-0 text-xs text-ink-5">
                These make a new key in slot 9a. If you already use 9a for something else, don't
                run them.
              </p>
            {/if}
            <div class="mt-3 flex gap-2">
              <button class={ui.secondary} onclick={() => chosen && choose(chosen)}>Try again</button>
              <button class={ui.secondary} onclick={() => (step = "choose")}>Back</button>
            </div>
          {/if}
        {:else if step === "confirm" && plan}
          <div class="font-medium text-ink">Switch to this key?</div>
          <div class="mt-1 space-y-2">
            {#if plan.plan === "same"}
              <p class="m-0">You're already using this key.</p>
            {:else if plan.plan === "bind"}
              <p class="m-0">Your mesh already knows this key. Director will use it from now on.</p>
            {:else if plan.plan === "add-admin"}
              <p class="m-0">
                Because you started this mesh, your original key can't be replaced. So this key joins
                as a second admin, <strong>{plan.newUsername}</strong>, and Director uses it from now on.
              </p>
              <p class="m-0">
                Your original key is kept in a file next to it, as a spare. Put that spare somewhere
                safe and off this computer — it's how you get back in if you lose your security key.
              </p>
            {:else if plan.plan === "rotate"}
              <p class="m-0">
                Your account, <strong>{plan.username}</strong>, moves onto this key and keeps all its
                roles. Your current key stops working everywhere — on other computers too.
              </p>
            {:else}
              <p class="m-0">{plan.message}</p>
              {#if plan.plan === "ask-admin"}
                <p class="mono selectable m-0 text-2xs break-all text-ink-5">{plan.key}</p>
              {/if}
            {/if}
          </div>
          <div class="mt-3 flex gap-2">
            {#if ["bind", "add-admin", "rotate"].includes(plan.plan)}
              <button class={ui.primary} disabled={busy} onclick={carryOut}>
                {busy ? "Switching…" : "Switch to this key"}
              </button>
            {/if}
            <button class={ui.secondary} disabled={busy} onclick={closeSetup}>
              {["bind", "add-admin", "rotate"].includes(plan.plan) ? "Cancel" : "Close"}
            </button>
          </div>
        {:else if step === "done"}
          <div class="font-medium text-ink"><span style="color: var(--accent-ink);">✓</span> All set</div>
          <p class="mt-1 mb-0">You're now using your security key.</p>
          {#if bound?.movedTo}
            <p class="mt-2 mb-0">
              Your original key was kept as <span class="mono selectable break-all">{bound.movedTo}</span>.
              Please move it somewhere safe, off this computer — it's your spare.
            </p>
          {/if}
          <button class="linkish mt-3 text-xs" onclick={closeSetup}>Close</button>
        {/if}
      </div>
    {/if}

    {#if error && step !== "checking"}
      <div class={ui.errorBox} role="alert">{error}</div>
    {/if}

    <!-- ═══ the proof, for the curious ═══ -->
    {#if onKey && !onFile && st.reachable && step === "closed"}
      <div class="mt-4">
        {#if proof}
          {#if withdrawing}
            <div class="flex flex-wrap items-center gap-2 text-xs">
              <span class="text-ink-4">Your mesh will stop showing your key as proven.</span>
              <button class={ui.rowDanger} disabled={busy} onclick={withdraw}>Withdraw</button>
              <button class={ui.rowAction} disabled={busy} onclick={() => (withdrawing = false)}>Cancel</button>
            </div>
          {:else}
            <button class="linkish text-xs" onclick={() => (withdrawing = true)}>Withdraw the proof…</button>
            <span class="ml-2 text-xs text-ink-5">published {ago(proof.attestedAtMicros)}</span>
          {/if}
        {:else if !proofOpen}
          <button class="linkish text-xs" onclick={() => (proofOpen = true)}>
            Prove it's a real security key (optional)
          </button>
        {:else}
          <div class="rounded-md bg-surface-2 px-4 py-3.5 text-sm leading-relaxed text-ink-3">
            <p class="m-0">
              Your YubiKey can prove the key was made on it and can't be copied. Export the proof in
              a terminal:
            </p>
            <pre class="mono selectable mt-2 mb-0 overflow-x-auto rounded bg-surface px-3 py-2 text-2xs text-ink-2">{PROOF}</pre>
            <button class="linkish mt-1 text-xs" onclick={() => copy(PROOF)}>Copy</button>
            <p class="mt-2 mb-0 text-xs text-ink-5">
              An admin needs to trust Yubico first, once, in Access › Machines › Security key makers.
            </p>
            <div class="mt-3">
              {#each certs as c, i (c.token)}
                <div class="mono text-xs text-ink-3">{i + 1}. {c.name}</div>
              {/each}
              <button class="linkish text-xs" onclick={pickCert}>
                {certs.length ? "Add intermediate.der…" : "Choose leaf.der…"}
              </button>
            </div>
            <details class="mt-2 text-xs text-ink-5">
              <summary class="cursor-pointer">Not a YubiKey PIV key?</summary>
              <div class="mt-2 flex gap-3">
                <label class="block">
                  <span class={ui.label}>Kind</span>
                  <select bind:value={kind} class={ui.input}>
                    <option value="piv">PIV</option>
                    <option value="openpgp">OpenPGP</option>
                    <option value="fido2">FIDO2</option>
                  </select>
                </label>
                <label class="block">
                  <span class={ui.label}>Slot</span>
                  <input bind:value={slot} class={ui.inputMono} />
                </label>
              </div>
            </details>
            <div class="mt-3 flex gap-2">
              <button class={ui.primary} disabled={busy || certs.length === 0} onclick={publish}>
                {busy ? "Publishing…" : "Publish the proof"}
              </button>
              <button class={ui.secondary} disabled={busy} onclick={() => { proofOpen = false; certs = []; }}>
                Cancel
              </button>
            </div>
          </div>
        {/if}
      </div>
    {/if}

    <!-- ═══ old copies lying about ═══ -->
    {#if st.beside.length > 0}
      <p class="mt-4 mb-0 text-xs leading-relaxed text-ink-5">
        Next to your key file:
        {#each st.beside as name, i (name)}<span class="mono">{name}</span>{i < st.beside.length - 1 ? ", " : ""}{/each}.
        These may be old copies of a key. If you don't need them, move them off this computer or
        delete them.
      </p>
    {/if}
  {/if}
</div>
