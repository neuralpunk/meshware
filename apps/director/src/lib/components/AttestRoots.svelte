<script lang="ts">
  // Access → Machines → "Security key makers" (Y2/Y3, rule D-E3).
  //
  // `meshnode users attest-root`: the certificate authorities whose word
  // this mesh takes that a key was made on a device and cannot leave it.
  // Beside Identity providers because it is the same kind of decision —
  // whose statements about a person this mesh believes — and an admin
  // makes it once.
  //
  // The roots are replicated, not compiled in. A mesh that trusts none
  // verifies no proofs and shows no hardware badges: an unverified chain
  // is worth less than no chain, because it looks like proof.
  import { api, errText, type AttestRootView } from "../api";
  import { app, ago, ui } from "../state.svelte";
  import Hint from "./Hint.svelte";

  let roots = $state<AttestRootView[]>([]);
  let loaded = $state(false);
  let error = $state<string | null>(null);
  let busy = $state(false);

  let adding = $state(false);
  let cert = $state<{ token: string; name: string } | null>(null);
  let label = $state("");
  let removing = $state<AttestRootView | null>(null);

  async function load() {
    try {
      roots = await api.attestRoots();
      error = null;
    } catch (e) {
      error = errText(e);
    } finally {
      loaded = true;
    }
  }

  $effect(() => {
    void load();
  });

  const live = $derived(roots.filter((r) => !r.removed));

  async function pick() {
    const picked = await api.pickFile("The authority's certificate (DER)", ["der", "crt", "cer"]).catch((e) => {
      error = errText(e);
      return null;
    });
    if (picked) {
      cert = picked;
      if (!label.trim()) label = picked.name.replace(/\.(der|crt|cer)$/i, "");
    }
  }

  async function add() {
    if (!cert) return;
    busy = true;
    error = null;
    try {
      await api.attestRootAdd(cert.token, label.trim());
      adding = false;
      cert = null;
      label = "";
      await load();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }

  async function remove(r: AttestRootView) {
    busy = true;
    error = null;
    try {
      await api.attestRootRemove(r.id);
      removing = null;
      await load();
    } catch (e) {
      error = errText(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="card mt-4 px-[22px] py-4">
  <div class="flex items-baseline gap-2">
    <h2 class="m-0 text-base font-semibold">Security key makers</h2>
    <Hint
      text="The authorities whose certificates prove a key was made on a device and cannot be copied off it — Yubico's, or your own if you issue PIV cards. Someone who moves their key onto a security key publishes that proof, and every node checks it against this list."
      label="What these are"
    />
    {#if !adding}
      <button class="{ui.rowAction} ml-auto" disabled={!app.isAdmin} onclick={() => (adding = true)}>
        Trust one…
      </button>
    {/if}
  </div>
  <p class="mt-1 mb-0 text-xs leading-relaxed text-ink-5">
    With none, nobody's key can be shown as held on hardware — a proof nobody can check is worse
    than no proof, because it looks like one.
  </p>

  {#if adding}
    <div class="mt-3 space-y-3 rounded-md bg-surface-2 px-4 py-3.5">
      <p class="m-0 text-xs leading-relaxed text-ink-5">
        For YubiKeys, Yubico publishes its attestation authorities at
        <span class="mono">developers.yubico.com/PKI</span>. Convert a PEM file first:
        <span class="mono">openssl x509 -in ca.pem -outform der -out ca.der</span>.
      </p>
      <div>
        {#if cert}
          <span class="mono text-xs text-ink-3">{cert.name}</span>
          <button class="linkish ml-2 text-xs" onclick={pick}>choose another…</button>
        {:else}
          <button class="linkish text-xs" onclick={pick}>Choose the certificate…</button>
        {/if}
      </div>
      <label class="block">
        <span class={ui.label}>What to call it</span>
        <input bind:value={label} placeholder="Yubico PIV Attestation CA" class={ui.input} />
      </label>
      <div class="flex gap-2">
        <button class={ui.primary} disabled={busy || !cert} onclick={add}>{busy ? "Adding…" : "Trust it"}</button>
        <button class={ui.secondary} disabled={busy} onclick={() => { adding = false; cert = null; label = ""; }}>
          Cancel
        </button>
      </div>
    </div>
  {/if}

  {#if error}<div class="mt-2 text-xs text-error">{error}</div>{/if}

  {#if loaded && live.length === 0 && !adding}
    <p class="mt-3 mb-0 text-sm text-ink-4">None trusted.</p>
  {:else if live.length > 0}
    <div class="mt-3 flex flex-col gap-2">
      {#each live as r (r.id)}
        <div class="flex flex-wrap items-center gap-3 rounded-md bg-surface-2 px-3 py-2">
          <span class="min-w-0">
            <span class="block text-sm text-ink-2">{r.label}</span>
            <span class="block text-2xs text-ink-5">
              <span class="mono">{r.shortId}</span> · added by {r.addedBy} {ago(r.addedAtMicros)} ·
              vouches for {r.attesting} {r.attesting === 1 ? "person" : "people"}
            </span>
          </span>
          {#if removing?.id === r.id}
            <span class="ml-auto flex items-center gap-2">
              <span class="text-xs text-ink-4">
                {r.attesting ? `${r.attesting} proof${r.attesting === 1 ? "" : "s"} stop verifying.` : ""}
              </span>
              <button class={ui.rowDanger} disabled={busy} onclick={() => remove(r)}>Stop trusting</button>
              <button class={ui.rowAction} disabled={busy} onclick={() => (removing = null)}>Cancel</button>
            </span>
          {:else}
            <button class="linkish ml-auto text-xs" disabled={!app.isAdmin} onclick={() => (removing = r)}>
              Stop trusting…
            </button>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>
